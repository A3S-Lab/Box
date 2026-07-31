use std::collections::BTreeSet;
use std::sync::Arc;

use a3s_box_core::OperationId;
use serde_json::Value;

use super::*;
use crate::oci::build::{
    build, BoxBuildOptions, BoxBuildPlan, BuildCachePolicy, BuildOperationIdentity,
    BuildOutputReceipt, OCI_IMAGE_INDEX_MEDIA_TYPE,
};
use crate::oci::ImageStore;

const STORE_CAPACITY: u64 = 64 * 1024 * 1024;

fn source_digest() -> String {
    format!("sha256:{}", "a".repeat(64))
}

fn plan(platform: &str) -> BoxBuildPlan {
    plan_with_cache(platform, "disabled")
}

fn plan_with_cache(platform: &str, cache: &str) -> BoxBuildPlan {
    BoxBuildPlan::parse_acl(&format!(
        r#"
build "oci" {{
  cache = "{cache}"
  context = "."
  file = "Dockerfile"
  network = "none"
  platform = "{platform}"
  schema = "a3s.box.build-plan.v1"
}}
"#
    ))
    .expect("valid assembly fixture plan")
}

fn write_source(root: &std::path::Path) {
    std::fs::create_dir_all(root).expect("source directory");
    std::fs::write(
        root.join("Dockerfile"),
        "FROM scratch\nCOPY payload /payload\n",
    )
    .expect("Dockerfile");
    std::fs::write(root.join("payload"), "shared-platform-payload\n").expect("payload");
}

async fn recorded_input(
    operation: &str,
    platform: &str,
    source: &std::path::Path,
    store: Arc<ImageStore>,
) -> BuildOutputAssemblyInput {
    let plan = plan(platform);
    let identity = BuildOperationIdentity::new(
        OperationId::new(operation).expect("operation ID"),
        source_digest(),
    )
    .expect("build identity");
    let output = build(
        plan.compile(
            source,
            BoxBuildOptions {
                tag: Some(identity.output_reference().to_string()),
                quiet: true,
            },
        )
        .expect("compiled plan"),
        store,
    )
    .await
    .expect("single-platform fixture build");
    let receipt = BuildOutputReceipt::from_result(
        &identity,
        plan.canonical_digest().expect("plan digest"),
        &output,
        BuildCachePolicy::Disabled,
        None,
    )
    .expect("recorded output receipt");
    BuildOutputAssemblyInput::new(plan, receipt)
}

async fn fixture() -> (
    tempfile::TempDir,
    Arc<ImageStore>,
    BuildOutputAssemblyInput,
    BuildOutputAssemblyInput,
) {
    let temporary = tempfile::tempdir().expect("assembly fixture");
    let source = temporary.path().join("source");
    write_source(&source);
    let store = Arc::new(
        ImageStore::new(&temporary.path().join("images"), STORE_CAPACITY).expect("image store"),
    );
    let amd64 = recorded_input("assembly-amd64", "linux/amd64", &source, Arc::clone(&store)).await;
    let arm64 = recorded_input("assembly-arm64", "linux/arm64", &source, Arc::clone(&store)).await;
    (temporary, store, amd64, arm64)
}

fn manifest_platforms(result: &MultiPlatformBuildResult) -> Vec<String> {
    let digest = result
        .descriptor
        .digest
        .strip_prefix("sha256:")
        .expect("root SHA-256 digest");
    let bytes = std::fs::read(
        result
            .layout_directory
            .join("blobs")
            .join("sha256")
            .join(digest),
    )
    .expect("root image index");
    let index: Value = serde_json::from_slice(&bytes).expect("root image index JSON");
    index["manifests"]
        .as_array()
        .expect("manifest descriptors")
        .iter()
        .map(|descriptor| {
            let platform = &descriptor["platform"];
            format!(
                "{}/{}",
                platform["os"].as_str().expect("platform OS"),
                platform["architecture"]
                    .as_str()
                    .expect("platform architecture")
            )
        })
        .collect()
}

fn blob_names(layout: &std::path::Path) -> BTreeSet<String> {
    std::fs::read_dir(layout.join("blobs").join("sha256"))
        .expect("blob directory")
        .map(|entry| {
            entry
                .expect("blob entry")
                .file_name()
                .into_string()
                .expect("UTF-8 blob name")
        })
        .collect()
}

#[tokio::test]
async fn two_recorded_outputs_produce_one_deterministic_sorted_index() {
    let (_temporary, store, amd64, arm64) = fixture().await;
    let reversed = BuildOutputAssembly::new(
        "example.test/a3s/app:reversed",
        source_digest(),
        vec![arm64.clone(), amd64.clone()],
    )
    .expect("valid reversed assembly");
    let ordered = BuildOutputAssembly::new(
        "example.test/a3s/app:ordered",
        source_digest(),
        vec![amd64, arm64],
    )
    .expect("valid ordered assembly");

    let reversed_result = assemble_recorded_build_outputs(&reversed, Arc::clone(&store))
        .await
        .expect("reversed assembly");
    let ordered_result = assemble_recorded_build_outputs(&ordered, Arc::clone(&store))
        .await
        .expect("ordered assembly");

    assert_eq!(
        reversed_result.descriptor.media_type,
        OCI_IMAGE_INDEX_MEDIA_TYPE
    );
    assert_eq!(reversed_result.descriptor, ordered_result.descriptor);
    assert_eq!(reversed_result.platforms, ordered_result.platforms);
    assert_eq!(
        manifest_platforms(&reversed_result),
        vec!["linux/amd64", "linux/arm64"]
    );
}

#[tokio::test]
async fn duplicate_platforms_are_rejected_before_publication() {
    let (_temporary, store, amd64, _arm64) = fixture().await;
    let target = "example.test/a3s/app:duplicate";
    let error = BuildOutputAssembly::new(target, source_digest(), vec![amd64.clone(), amd64])
        .expect_err("duplicate platform must fail");

    assert!(error.to_string().contains("unique"));
    assert!(store.get(target).await.is_none());
}

#[tokio::test]
async fn plan_and_receipt_platform_mismatch_is_rejected() {
    let (_temporary, store, amd64, arm64) = fixture().await;
    let mismatched = BuildOutputAssemblyInput::new(arm64.plan().clone(), amd64.receipt().clone());
    let target = "example.test/a3s/app:mismatch";
    let error = BuildOutputAssembly::new(target, source_digest(), vec![amd64, mismatched])
        .expect_err("plan and receipt mismatch must fail");

    assert!(error.to_string().contains("plan"));
    assert!(store.get(target).await.is_none());
}

#[tokio::test]
async fn non_platform_build_intent_drift_is_rejected() {
    let (_temporary, store, amd64, arm64) = fixture().await;
    let drifted = BuildOutputAssemblyInput::new(
        plan_with_cache("linux/arm64", "content-addressed"),
        arm64.receipt().clone(),
    );
    let target = "example.test/a3s/app:intent-drift";
    let error = BuildOutputAssembly::new(target, source_digest(), vec![amd64, drifted])
        .expect_err("non-platform intent drift must fail");

    assert!(error.to_string().contains("non-platform build intent"));
    assert!(store.get(target).await.is_none());
}

#[tokio::test]
async fn receipt_tampering_is_rejected_before_publication() {
    let (_temporary, store, amd64, arm64) = fixture().await;
    let mut receipt = amd64.receipt().clone();
    receipt.output.descriptor.digest = format!("sha256:{}", "b".repeat(64));
    let tampered = BuildOutputAssemblyInput::new(amd64.plan().clone(), receipt);
    let target = "example.test/a3s/app:receipt-tamper";
    let assembly = BuildOutputAssembly::new(target, source_digest(), vec![tampered, arm64])
        .expect("path-independent receipt shape remains valid");
    let error = assemble_recorded_build_outputs(&assembly, Arc::clone(&store))
        .await
        .expect_err("tampered receipt must fail revalidation");

    assert!(error.to_string().contains("receipt") || error.to_string().contains("output"));
    assert!(store.get(target).await.is_none());
}

#[tokio::test]
async fn input_layout_tampering_leaves_no_target_publication() {
    let (_temporary, store, amd64, arm64) = fixture().await;
    let assembly = BuildOutputAssembly::new(
        "example.test/a3s/app:layout-tamper",
        source_digest(),
        vec![amd64.clone(), arm64],
    )
    .expect("valid assembly");
    let stored = store
        .get(&amd64.receipt().output.reference)
        .await
        .expect("recorded input");
    let blob = std::fs::read_dir(stored.path.join("blobs").join("sha256"))
        .expect("input blobs")
        .next()
        .expect("one input blob")
        .expect("blob entry")
        .path();
    let mut bytes = std::fs::read(&blob).expect("input blob bytes");
    bytes[0] ^= 1;
    std::fs::write(blob, bytes).expect("tampered input blob");

    assemble_recorded_build_outputs(&assembly, Arc::clone(&store))
        .await
        .expect_err("tampered input layout must fail");
    assert!(store.get(assembly.reference()).await.is_none());
}

#[tokio::test]
async fn shared_blobs_are_copied_once_into_the_assembled_layout() {
    let (_temporary, store, amd64, arm64) = fixture().await;
    let mut source_blobs = BTreeSet::new();
    for input in [&amd64, &arm64] {
        let stored = store
            .get(&input.receipt().output.reference)
            .await
            .expect("recorded input");
        source_blobs.extend(blob_names(&stored.path));
    }
    let assembly = BuildOutputAssembly::new(
        "example.test/a3s/app:deduplicated",
        source_digest(),
        vec![amd64, arm64],
    )
    .expect("valid assembly");

    let result = assemble_recorded_build_outputs(&assembly, Arc::clone(&store))
        .await
        .expect("assembled output");
    let assembled_blobs = blob_names(&result.layout_directory);

    assert_eq!(assembled_blobs.len(), source_blobs.len() + 1);
    assert!(source_blobs.is_subset(&assembled_blobs));
    assert_eq!(result.blob_count, assembled_blobs.len());
}

#[tokio::test]
async fn repeated_publication_is_idempotent_in_the_one_image_store() {
    let (_temporary, store, amd64, arm64) = fixture().await;
    let assembly = BuildOutputAssembly::new(
        "example.test/a3s/app:idempotent",
        source_digest(),
        vec![amd64, arm64],
    )
    .expect("valid assembly");

    let first = assemble_recorded_build_outputs(&assembly, Arc::clone(&store))
        .await
        .expect("first assembly");
    let second = assemble_recorded_build_outputs(&assembly, Arc::clone(&store))
        .await
        .expect("idempotent assembly");

    assert_eq!(first.descriptor, second.descriptor);
    assert_eq!(first.layout_directory, second.layout_directory);
    assert_eq!(first.blob_inventory_digest, second.blob_inventory_digest);
    assert_eq!(store.list().await.len(), 3);
}
