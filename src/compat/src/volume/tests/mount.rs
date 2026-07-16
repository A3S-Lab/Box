use super::super::*;

#[test]
fn mount_validation_rejects_ambiguous_or_overlapping_destinations() {
    for path in ["", "/", "relative", "/a/../b", "/a/./b", "/a:b"] {
        assert!(VolumeMount::new("data", path).is_err(), "accepted {path:?}");
    }
    assert!(VolumeMount::new("../data", "/mnt/data").is_err());
    assert!(validate_mounts(&[
        VolumeMount::new("one", "/mnt/data").unwrap(),
        VolumeMount::new("two", "/mnt/data").unwrap(),
    ])
    .is_err());
}
