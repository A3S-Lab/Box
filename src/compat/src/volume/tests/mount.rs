use super::super::*;

#[test]
fn mount_validation_rejects_ambiguous_or_overlapping_destinations() {
    for path in [
        "",
        "/",
        "relative",
        "/a/../b",
        "/a/./b",
        "/a:b",
        "//mnt/data",
        "/mnt/data/",
        "/mnt\\data",
        "/mnt/\0data",
    ] {
        assert!(VolumeMount::new("data", path).is_err(), "accepted {path:?}");
    }
    assert!(VolumeMount::new("../data", "/mnt/data").is_err());
    assert!(validate_mounts(&[
        VolumeMount::new("one", "/mnt/data").unwrap(),
        VolumeMount::new("two", "/mnt/data").unwrap(),
    ])
    .is_err());
}

#[test]
fn mount_validation_uses_guest_posix_semantics_on_every_host() {
    let mount = VolumeMount::new("data", "/mnt/nested/data").unwrap();
    assert_eq!(mount.path, "/mnt/nested/data");
}
