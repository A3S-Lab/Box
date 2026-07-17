use super::*;

#[test]
fn target_platform_defaults_to_linux_host_architecture() {
    let platform = target_platform(None).unwrap();
    assert_eq!(platform.os, "linux");
    assert_eq!(platform.architecture, Platform::host().architecture);
}

#[test]
fn target_platform_normalizes_architecture_aliases() {
    assert_eq!(
        target_platform(Some("linux/x86_64")).unwrap(),
        Platform::linux_amd64()
    );
    assert_eq!(
        target_platform(Some("linux/aarch64")).unwrap(),
        Platform::linux_arm64()
    );
}

#[test]
fn target_platform_rejects_empty_and_non_linux_values() {
    assert_eq!(
        target_platform(Some("  ")).unwrap_err(),
        "Image platform cannot be empty"
    );
    assert!(target_platform(Some("windows/amd64"))
        .unwrap_err()
        .contains("executes Linux images"));
}

#[test]
fn load_reference_prefers_tag_then_selected_then_root_annotation() {
    let selected: Descriptor = serde_json::from_value(serde_json::json!({
        "mediaType": OCI_IMAGE_MANIFEST,
        "digest": format!("sha256:{}", "a".repeat(64)),
        "size": 1,
        "annotations": { IMAGE_REF_ANNOTATION: "selected:latest" }
    }))
    .unwrap();
    let root = serde_json::json!({
        "manifests": [{
            "annotations": { IMAGE_REF_ANNOTATION: "root:latest" }
        }]
    });
    let digest = selected.digest().to_string();

    assert_eq!(
        load_reference(&root, &selected, Some(" explicit:latest "), &digest).unwrap(),
        "explicit:latest"
    );
    assert_eq!(
        load_reference(&root, &selected, None, &digest).unwrap(),
        "selected:latest"
    );

    let without_annotation: Descriptor = serde_json::from_value(serde_json::json!({
        "mediaType": OCI_IMAGE_MANIFEST,
        "digest": digest.clone(),
        "size": 1
    }))
    .unwrap();
    assert_eq!(
        load_reference(&root, &without_annotation, None, &digest).unwrap(),
        "root:latest"
    );
    assert_eq!(
        load_reference(
            &serde_json::json!({"manifests": [{}]}),
            &without_annotation,
            None,
            &digest,
        )
        .unwrap(),
        digest
    );
    assert_eq!(
        load_reference(&root, &selected, Some("  "), &digest).unwrap_err(),
        "Image tag cannot be empty"
    );
}
