use crate::volume::GenericVolumeIdentity;
use dedupe_core::traits::VolumeIdentity;
use std::fs;
use std::path::PathBuf;

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dedupe-identify-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[test]
fn identify_should_return_non_empty_uuid_when_path_is_temp_dir() {
    let dir = scratch_dir("nonempty");
    let info = GenericVolumeIdentity
        .identify(&dir)
        .expect("identify temp dir");
    assert!(!info.uuid.is_empty(), "uuid must not be empty");
    fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn identify_should_return_same_uuid_and_mount_when_called_twice() {
    let dir = scratch_dir("stable");
    let a = GenericVolumeIdentity
        .identify(&dir)
        .expect("first identify");
    let b = GenericVolumeIdentity
        .identify(&dir)
        .expect("second identify");
    assert_eq!(a.uuid, b.uuid, "uuid must be stable across calls");
    assert_eq!(a.mount, b.mount, "mount must be stable across calls");
    fs::remove_dir_all(&dir).expect("cleanup");
}
