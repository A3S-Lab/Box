use super::validation::{require_descriptor, validate_config};
use super::*;

fn payload_descriptor(platform: bool, artifact_type: bool) -> Descriptor {
    let mut builder = DescriptorBuilder::default()
        .media_type(MediaType::ImageLayerGzip)
        .digest(parse_digest(&"a".repeat(64)).unwrap())
        .size(1_u64);
    if platform {
        builder = builder.platform(
            PlatformBuilder::default()
                .architecture(Arch::from("amd64"))
                .os(Os::from("linux"))
                .build()
                .unwrap(),
        );
    }
    if artifact_type {
        builder = builder.artifact_type(MediaType::from(BUILD_CACHE_ARTIFACT_MEDIA_TYPE));
    }
    builder.build().unwrap()
}

#[test]
fn cache_payload_descriptors_reject_root_only_identity_fields() {
    assert!(require_descriptor(
        &payload_descriptor(false, false),
        MediaType::ImageLayerGzip,
        false,
    )
    .is_ok());
    assert!(require_descriptor(
        &payload_descriptor(true, false),
        MediaType::ImageLayerGzip,
        false,
    )
    .is_err());
    assert!(require_descriptor(
        &payload_descriptor(false, true),
        MediaType::ImageLayerGzip,
        false,
    )
    .is_err());
}

#[test]
fn cache_manifest_layers_are_unique_and_canonical() {
    let platform = Platform::linux_amd64();
    let identity = BuildCacheExportIdentity::new(
        format!("sha256:{}", "1".repeat(64)),
        format!("sha256:{}", "2".repeat(64)),
        platform.clone(),
    )
    .unwrap();
    let layer = build_descriptor(MediaType::ImageLayerGzip, 1, &"3".repeat(64)).unwrap();
    let entry = CacheEntry {
        key: format!("sha256:{}", "4".repeat(64)),
        layer: BuildOutputDescriptor {
            media_type: layer.media_type().as_ref().to_string(),
            digest: layer.digest().to_string(),
            size: layer.size(),
        },
        diff_id: format!("sha256:{}", "5".repeat(64)),
    };
    let config = CacheConfig {
        schema: CACHE_CONFIG_SCHEMA.to_string(),
        key: identity.key.clone(),
        source_digest: identity.source_digest.clone(),
        plan_digest: identity.plan_digest.clone(),
        platform,
        entries: vec![entry],
    };

    assert!(validate_config(&config, &identity, std::slice::from_ref(&layer)).is_ok());
    assert!(validate_config(&config, &identity, &[layer.clone(), layer.clone()]).is_err());

    let mut shared_layer = config.clone();
    let mut second = shared_layer.entries[0].clone();
    second.key = format!("sha256:{}", "6".repeat(64));
    shared_layer.entries.push(second.clone());
    assert!(validate_config(&shared_layer, &identity, std::slice::from_ref(&layer)).is_ok());

    second.diff_id = format!("sha256:{}", "7".repeat(64));
    shared_layer.entries[1] = second;
    assert!(validate_config(&shared_layer, &identity, std::slice::from_ref(&layer)).is_err());
}
