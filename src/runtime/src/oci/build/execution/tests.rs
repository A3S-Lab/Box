use std::sync::Arc;

use super::*;
use crate::OCI_IMAGE_MANIFEST_MEDIA_TYPE;

#[tokio::test]
async fn execution_returns_a_plan_bound_typed_oci_output() {
    let temporary = tempfile::TempDir::new().unwrap();
    let source = temporary.path().join("source");
    let store_root = temporary.path().join("images");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(
        source.join("Dockerfile"),
        "FROM scratch\nCOPY value /value\n",
    )
    .unwrap();
    std::fs::write(source.join("value"), "typed-output\n").unwrap();

    let platform = if cfg!(target_arch = "aarch64") {
        "linux/arm64"
    } else {
        "linux/amd64"
    };
    let plan = BoxBuildPlan::parse_acl(&format!(
        r#"
build "oci" {{
  cache = "disabled"
  context = "."
  file = "Dockerfile"
  network = "none"
  platform = "{platform}"
  schema = "a3s.box.build-plan.v1"
}}
"#
    ))
    .unwrap();
    let expected_plan_digest = plan.canonical_digest().unwrap();
    let store = Arc::new(ImageStore::new(&store_root, 64 * 1024 * 1024).unwrap());

    let result = execute_build_plan(
        &plan,
        &source,
        BoxBuildOptions {
            tag: Some("a3s-cloud/build:test".into()),
            quiet: true,
        },
        Arc::clone(&store),
    )
    .await
    .unwrap();

    assert_eq!(result.plan_digest, expected_plan_digest);
    assert_eq!(result.output.reference, "a3s-cloud/build:test");
    assert_eq!(result.output.platform, *plan.platform());
    assert_eq!(
        result.output.descriptor.media_type,
        OCI_IMAGE_MANIFEST_MEDIA_TYPE
    );
    assert_eq!(result.output.descriptor.digest, result.output.digest);
    assert!(result.output.descriptor.size > 0);
    assert_eq!(result.output.layer_count, 1);
    assert_eq!(result.output.blob_count, 3);
    assert!(result.output.content_bytes() >= result.output.descriptor.size);
    assert!(result
        .output
        .layout_directory
        .starts_with(store_root.canonicalize().unwrap()));
    assert!(result.output.layout_directory.join("oci-layout").is_file());
    assert!(result.output.layout_directory.join("index.json").is_file());
    crate::oci::OciImage::from_path(&result.output.layout_directory).unwrap();

    let stored = store.get("a3s-cloud/build:test").await.unwrap();
    assert_eq!(stored.digest, result.output.descriptor.digest);
    assert_eq!(
        stored.path.canonicalize().unwrap(),
        result.output.layout_directory
    );
    assert_eq!(stored.size_bytes, result.output.content_bytes());
}
