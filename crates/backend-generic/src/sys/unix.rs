//! Unix bits shared by the Linux and macOS implementations (and unix targets that fall
//! back to `other`).

#![expect(
    unsafe_code,
    reason = "fcntl on a raw descriptor; each unsafe block states its invariants"
)]

use std::fs::{File, FileType, Metadata, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use dedupe_core::model::attrs;
use dedupe_core::path::RawPath;
use dedupe_core::traits::FileMeta;

use std::ffi::{OsStr, OsString};

/// Walk up until the device id changes; that is the mount root.
pub(super) fn mount_root(path: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    let mut cur = path.to_path_buf();
    while let Some(parent) = cur.parent() {
        match (cur.metadata(), parent.metadata()) {
            (Ok(a), Ok(b)) if a.dev() == b.dev() => cur = parent.to_path_buf(),
            _ => break,
        }
    }
    cur
}

/// `(file_id, dev, nlink, HARDLINK bit)`. Unix reads the inode; elsewhere there is no
/// identity and the link count is 1.
pub(super) fn identity_bits(md: &Metadata) -> (Option<Vec<u8>>, Option<u64>, i64, i64) {
    use std::os::unix::fs::MetadataExt;
    let hard = if md.nlink() > 1 { attrs::HARDLINK } else { 0 };
    (
        Some(md.ino().to_le_bytes().to_vec()),
        Some(md.dev()),
        md.nlink() as i64,
        hard,
    )
}

pub(super) fn ctime_ns(md: &Metadata) -> Option<i64> {
    use dedupe_core::time::unix_parts_to_ns;
    use std::os::unix::fs::MetadataExt;
    unix_parts_to_ns(md.ctime(), md.ctime_nsec())
}

/// Unix: `O_NOFOLLOW` refuses a symlink swapped in after enumeration, and `O_NONBLOCK`
/// stops a FIFO swapped in the same way from hanging a worker.
pub(super) fn configure_open(opts: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    // O_CLOEXEC matches std's default and is stated here so the policy is explicit.
    opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
}

/// The non-blocking flag is cleared again after the open.
pub(super) fn after_open(file: &File) -> io::Result<()> {
    clear_nonblock(file)
}

/// Clears `O_NONBLOCK` on an open descriptor. Regular-file reads ignore the flag, so
/// clearing it only restores the default mode for later users of the handle.
fn clear_nonblock(file: &File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is owned by `file`, which is alive for both calls, so the descriptor is
    // open. F_GETFL takes no pointer argument.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: same descriptor and liveness as above. F_SETFL takes an int argument only,
    // so no memory is passed to the kernel.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// One positional read.
pub(super) fn read_once(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

pub(super) fn os_str_to_raw(path: &OsStr) -> RawPath {
    use std::os::unix::ffi::OsStrExt;
    RawPath::new(path.as_bytes().to_vec())
}

pub(super) fn raw_to_os_string(path: &RawPath) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(path.0.clone())
}

/// `md` is the whole answer.
pub(super) fn file_meta_at(_path: &Path, md: &Metadata, ft: FileType) -> FileMeta {
    crate::meta::file_meta_from(md, ft)
}

/// `lstat` (the same cost as before).
pub(super) fn walk_file_meta(entry: &walkdir::DirEntry, ft: FileType) -> io::Result<FileMeta> {
    let md = entry.path().symlink_metadata()?;
    Ok(crate::meta::file_meta_from(&md, ft))
}

/// Runs `cmd` and returns its stdout on a zero exit status, or `None` on spawn
/// failure, non-zero exit, or when `timeout` elapses (the child is then killed and reaped).
/// stdin and stderr are discarded. stdout is drained on a thread so a full pipe cannot
/// deadlock. On timeout that thread is detached rather than joined, in case a grandchild
/// still holds the pipe open.
// Unused only on unix targets other than macOS and Linux (both callers are cfg-gated to those).
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
pub(crate) fn output_with_timeout(
    mut cmd: std::process::Command,
    timeout: std::time::Duration,
) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        out.read_to_end(&mut buf).map(|_| buf)
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return reader.join().ok()?.ok(),
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                // Timed out or try_wait failed: kill and reap; reader thread detached.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn output_with_timeout_should_return_stdout_for_fast_command() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo hi"]);
        let out = super::output_with_timeout(cmd, std::time::Duration::from_secs(5));
        assert_eq!(
            out,
            Some(b"hi\n".to_vec()),
            "a fast successful command should return its stdout, got {out:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_with_timeout_should_return_none_when_deadline_passes() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 10"]);
        let started = std::time::Instant::now();
        let out = super::output_with_timeout(cmd, std::time::Duration::from_millis(100));
        let elapsed = started.elapsed();
        assert_eq!(
            out, None,
            "a command past its deadline should give None, got {out:?}"
        );
        // The spec expects about 1 s; the bound is loose so a loaded CI host does not flake.
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "timeout should return well before the 10 s sleep ends, took {elapsed:?}"
        );
    }
}

#[cfg(all(test, unix))]
mod mount_root_tests {
    use super::mount_root;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("dedupe-mount-root-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn dev(p: &Path) -> u64 {
        p.metadata().expect("metadata").dev()
    }

    #[test]
    fn mount_root_should_return_ancestor_on_same_device_when_path_is_nested() {
        let base = scratch_dir("nested");
        let nested = base.join("a").join("b").join("c");
        fs::create_dir_all(&nested).expect("create nested dirs");

        let root = mount_root(&nested);

        assert!(
            nested.starts_with(&root),
            "{root:?} is not an ancestor of {nested:?}"
        );
        assert_eq!(dev(&root), dev(&nested), "root must share the device");
        if let Some(parent) = root.parent() {
            if let Ok(pm) = parent.metadata() {
                assert_ne!(pm.dev(), dev(&root), "climb must stop at a device change");
            }
        }
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn mount_root_should_return_root_unchanged_when_path_is_filesystem_root() {
        assert_eq!(mount_root(Path::new("/")), PathBuf::from("/"));
    }
}

#[cfg(test)]
mod codec_tests {
    use crate::codec::GenericPathCodec;
    use dedupe_core::path::RawPath;
    use dedupe_core::traits::PathCodec;

    #[test]
    fn byte_exact_round_trip() {
        let codec = GenericPathCodec;
        let raw = RawPath::new(vec![0xff, 0xfe, b'/', 0x80, b'a']);
        let native = codec.to_native(&raw);
        assert_eq!(codec.to_raw(&native), raw);
    }
}
