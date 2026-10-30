//! macOS storage probe: `diskutil info -plist` for the `SolidState` flag of the device
//! behind the mount, plus filesystem locality.

#![expect(
    unsafe_code,
    reason = "libc statfs; each unsafe block states its invariants"
)]

use std::path::Path;

use dedupe_core::error::Result;

use crate::probe::FsLocality;

pub(super) fn is_rotational(path: &Path) -> Result<Option<bool>> {
    // diskutil reports the opposite polarity (SolidState), so the inversion
    // happens once, here, at the trait boundary.
    Ok(diskutil::solid_state(path).map(|solid_state| !solid_state))
}

pub(super) fn locality_macos(path: &Path) -> FsLocality {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return FsLocality::Unknown;
    };
    // SAFETY: `libc::statfs` is plain old data; all-zero is a valid value.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is NUL-terminated and outlives the call; `st` is writable.
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return FsLocality::Unknown;
    }
    if st.f_flags & (libc::MNT_LOCAL as u32) == 0 {
        FsLocality::Remote
    } else {
        FsLocality::Local
    }
}

/// macOS probe: `statfs` finds the device node behind the path, and `diskutil`
/// reports whether that device is solid state.
mod diskutil {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

    use crate::parse::plist::parse_solid_state;
    use crate::sys::unix::output_with_timeout;

    /// Absolute path so a hostile `PATH` cannot substitute another `diskutil`.
    const DISKUTIL: &str = "/usr/sbin/diskutil";

    /// Upper bound on the `diskutil` call so a wedged disk cannot hang the scan.
    const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

    /// `SolidState` flag of the device backing `path`, or `None` when unknown.
    pub(super) fn solid_state(path: &Path) -> Option<bool> {
        let device = mounted_device(path)?;
        let mut cmd = Command::new(DISKUTIL);
        cmd.args(["info", "-plist", device.as_str()]);
        let stdout = output_with_timeout(cmd, PROBE_TIMEOUT)?;
        let xml = std::str::from_utf8(&stdout).ok()?;
        parse_solid_state(xml)
    }

    /// Device node the filesystem holding `path` is mounted from, e.g. `/dev/disk3s1s1`.
    /// Network mounts report `//user@host/share`, so only `/dev/` sources are accepted.
    fn mounted_device(path: &Path) -> Option<String> {
        let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: `c_path` is NUL-terminated and outlives the call. `libc::statfs` writes
        // one struct into `st`, and the all-zero bit pattern is a valid value for this
        // plain C struct. `st` is only returned when the call reports success.
        let st = unsafe {
            let mut st: libc::statfs = std::mem::zeroed();
            (libc::statfs(c_path.as_ptr(), &mut st) == 0).then_some(st)
        }?;
        let name: Vec<u8> = st
            .f_mntfromname
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        let device = String::from_utf8(name).ok()?;
        device.starts_with("/dev/").then_some(device)
    }
}
