use std::path::Path;

#[cfg(unix)]
#[test]
fn fs_locality_should_return_unknown_when_path_missing() {
    let missing = Path::new("/definitely/not/a/real/dedupe/path");
    let locality = super::fs_locality(missing);
    assert_eq!(
        locality,
        super::FsLocality::Unknown,
        "a path that cannot be stat-ed should be Unknown, got {locality:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn fs_locality_should_not_report_remote_when_path_is_temp_dir() {
    let locality = super::fs_locality(&std::env::temp_dir());
    assert_ne!(
        locality,
        super::FsLocality::Remote,
        "the temp dir is normally local disk or tmpfs, got {locality:?}"
    );
}
