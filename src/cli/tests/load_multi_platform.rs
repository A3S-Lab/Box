use std::io::Write;
use std::path::Path;

use sha2::{Digest, Sha256};

mod support;
use support::CliTest;

const IMAGE_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const IMAGE_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

#[test]
fn load_selects_a_manifest_from_a_nested_multi_platform_index() {
    let cli = CliTest::new();
    let archive = cli.home_path().join("multi-platform.tar");
    let fixture = create_multi_platform_archive(&archive, "fixture:latest", false);
    let archive = archive.to_string_lossy().to_string();
    let reference = "loaded-arm64:latest";

    cli.ok(&[
        "load",
        "--input",
        &archive,
        "--tag",
        reference,
        "--platform",
        "linux/arm64",
    ]);

    let inspect: serde_json::Value =
        serde_json::from_str(&cli.ok(&["image-inspect", reference])).unwrap();
    let image = &inspect[0];
    assert_eq!(image["Reference"], reference);
    assert_eq!(image["Digest"], fixture.arm64_manifest_digest);
    assert_eq!(
        image["Config"]["Cmd"],
        serde_json::json!(["echo", "selected-arm64"])
    );

    cli.ok(&["rmi", "--force", reference]);
}

#[test]
fn load_rejects_an_unavailable_platform_before_publishing_the_tag() {
    let cli = CliTest::new();
    let archive = cli.home_path().join("multi-platform.tar");
    create_multi_platform_archive(&archive, "fixture:latest", false);
    let archive = archive.to_string_lossy().to_string();
    let reference = "missing-platform:latest";

    cli.fails(
        &[
            "load",
            "--input",
            &archive,
            "--tag",
            reference,
            "--platform",
            "linux/s390x",
        ],
        "No image manifest for linux/s390x",
    );
    assert!(!cli.ok(&["images", "--quiet"]).contains(reference));
}

#[test]
fn load_rejects_a_corrupt_selected_manifest_before_publishing_the_tag() {
    let cli = CliTest::new();
    let archive = cli.home_path().join("corrupt-multi-platform.tar");
    create_multi_platform_archive(&archive, "fixture:latest", true);
    let archive = archive.to_string_lossy().to_string();
    let reference = "corrupt-platform:latest";

    cli.fails(
        &[
            "load",
            "--input",
            &archive,
            "--tag",
            reference,
            "--platform",
            "linux/arm64",
        ],
        "OCI blob digest mismatch",
    );
    assert!(!cli.ok(&["images", "--quiet"]).contains(reference));
}

struct MultiPlatformFixture {
    arm64_manifest_digest: String,
}

fn create_multi_platform_archive(
    path: &Path,
    reference: &str,
    corrupt_arm64_manifest: bool,
) -> MultiPlatformFixture {
    let layout = tempfile::tempdir().unwrap();
    let blobs = layout.path().join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs).unwrap();

    let amd64 = write_platform_manifest(&blobs, "amd64", "selected-amd64");
    let arm64 = write_platform_manifest(&blobs, "arm64", "selected-arm64");
    let nested_index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": IMAGE_INDEX_MEDIA_TYPE,
        "manifests": [
            {
                "mediaType": IMAGE_MANIFEST_MEDIA_TYPE,
                "digest": amd64.digest.clone(),
                "size": amd64.size,
                "platform": { "os": "linux", "architecture": "amd64" }
            },
            {
                "mediaType": IMAGE_MANIFEST_MEDIA_TYPE,
                "digest": arm64.digest.clone(),
                "size": arm64.size,
                "platform": { "os": "linux", "architecture": "arm64", "variant": "v8" }
            }
        ]
    });
    let nested = write_json_blob(&blobs, &nested_index);

    if corrupt_arm64_manifest {
        let manifest_path = blobs.join(arm64.digest.strip_prefix("sha256:").unwrap());
        let mut bytes = std::fs::read(&manifest_path).unwrap();
        bytes[0] ^= 1;
        std::fs::write(manifest_path, bytes).unwrap();
    }

    std::fs::write(
        layout.path().join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .unwrap();
    let root_index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": IMAGE_INDEX_MEDIA_TYPE,
        "manifests": [{
            "mediaType": IMAGE_INDEX_MEDIA_TYPE,
            "digest": nested.digest.clone(),
            "size": nested.size,
            "annotations": {
                "org.opencontainers.image.ref.name": reference
            }
        }]
    });
    std::fs::write(
        layout.path().join("index.json"),
        serde_json::to_vec(&root_index).unwrap(),
    )
    .unwrap();

    let file = std::fs::File::create(path).unwrap();
    let mut archive = tar::Builder::new(file);
    archive
        .append_path_with_name(layout.path().join("oci-layout"), "oci-layout")
        .unwrap();
    archive
        .append_path_with_name(layout.path().join("index.json"), "index.json")
        .unwrap();
    archive
        .append_dir_all("blobs", layout.path().join("blobs"))
        .unwrap();
    archive.finish().unwrap();

    MultiPlatformFixture {
        arm64_manifest_digest: arm64.digest,
    }
}

fn write_platform_manifest(blobs: &Path, architecture: &str, marker: &str) -> BlobDescriptor {
    let config = serde_json::json!({
        "architecture": architecture,
        "os": "linux",
        "config": {
            "Cmd": ["echo", marker]
        },
        "rootfs": {
            "type": "layers",
            "diff_ids": []
        },
        "history": []
    });
    let config = write_json_blob(blobs, &config);
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": IMAGE_MANIFEST_MEDIA_TYPE,
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config.digest,
            "size": config.size
        },
        "layers": []
    });
    write_json_blob(blobs, &manifest)
}

struct BlobDescriptor {
    digest: String,
    size: usize,
}

fn write_json_blob(blobs: &Path, value: &serde_json::Value) -> BlobDescriptor {
    let bytes = serde_json::to_vec(value).unwrap();
    let hex = format!("{:x}", Sha256::digest(&bytes));
    let mut file = std::fs::File::create(blobs.join(&hex)).unwrap();
    file.write_all(&bytes).unwrap();
    BlobDescriptor {
        digest: format!("sha256:{hex}"),
        size: bytes.len(),
    }
}
