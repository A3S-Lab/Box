use super::*;

#[test]
fn create_request_defaults_to_a_local_microvm() {
    let request: BridgeRequest = serde_json::from_str(r#"{"operation":"sandbox_create"}"#).unwrap();
    let BridgeRequest::SandboxCreate(request) = request else {
        panic!("expected create request");
    };
    assert_eq!(request.image, DEFAULT_SANDBOX_IMAGE);
    assert_eq!(request.timeout_seconds, DEFAULT_SANDBOX_TIMEOUT_SECONDS);
    assert_eq!(request.isolation, ExecutionIsolation::Microvm);
}

#[test]
fn create_request_maps_language_options_to_the_runtime_facade() {
    let home = tempfile::tempdir().unwrap();
    let client = A3sBoxClient::from_home(home.path());
    let source_snapshot = ExecutionSnapshotId::new("ci-base-source").unwrap();
    let (request, _) = SandboxCreateOptions::new("python:3.12-alpine")
        .timeout_seconds(120)
        .env("MODE", "test")
        .metadata("suite", "sdk")
        .name("local-sdk")
        .cpus(4)
        .memory_mb(2048)
        .isolation(ExecutionIsolation::Sandbox)
        .filesystem_snapshot(source_snapshot.clone())
        .into_runtime_request(&client)
        .unwrap();

    assert_eq!(request.config.image, "python:3.12-alpine");
    assert_eq!(request.config.resources.timeout, 120);
    assert_eq!(request.config.resources.vcpus, 4);
    assert_eq!(request.config.resources.memory_mb, 2048);
    assert_eq!(request.config.isolation, ExecutionIsolation::Sandbox);
    assert_eq!(
        request.config.cmd,
        ["/bin/sh", "-c", "while :; do sleep 3600; done"]
    );
    assert_eq!(request.policy.name.as_deref(), Some("local-sdk"));
    assert!(request.policy.auto_remove);
    assert_eq!(request.labels.get("suite").map(String::as_str), Some("sdk"));
    assert_eq!(request.rootfs_snapshot_id, Some(source_snapshot));
}

#[test]
fn builder_maps_typed_storage_network_and_runtime_options() {
    let home = tempfile::tempdir().unwrap();
    let client = A3sBoxClient::from_home(home.path());
    client.volume("ci-cache").size_limit(4096).create().unwrap();
    client
        .network("ci-net")
        .subnet("10.89.77.0/24")
        .create()
        .unwrap();

    let (request, _) = SandboxCreateOptions::new("local/ci-base:latest")
        .mount(VolumeMount::named("ci-cache", "/cache").read_only(true))
        .tmpfs(TmpfsMount::new("/scratch").size_bytes(1024))
        .network(SandboxNetwork::bridge("ci-net"))
        .publish_port(PortMapping::tcp(8080, 80).unwrap())
        .workdir("/workspace")
        .user("1000:1000")
        .hostname("ci-runner")
        .dns_server("1.1.1.1")
        .host_alias("registry.local", "10.89.77.10")
        .read_only(true)
        .persistent(true)
        .auto_remove(false)
        .into_runtime_request(&client)
        .unwrap();

    assert_eq!(request.policy.volume_names, ["ci-cache"]);
    assert_eq!(request.config.network.to_string(), "bridge:ci-net");
    assert_eq!(request.config.port_map, ["8080:80"]);
    assert_eq!(request.config.tmpfs, ["/scratch:size=1024"]);
    assert_eq!(request.config.workdir.as_deref(), Some("/workspace"));
    assert_eq!(request.config.user.as_deref(), Some("1000:1000"));
    assert_eq!(request.config.hostname.as_deref(), Some("ci-runner"));
    assert_eq!(request.config.dns, ["1.1.1.1"]);
    assert_eq!(request.config.add_hosts, ["registry.local:10.89.77.10"]);
    assert!(request.config.volumes[0].ends_with(":/cache:ro"));
    assert!(request.config.read_only);
    assert!(request.config.persistent);
    assert!(!request.policy.auto_remove);
}

#[tokio::test]
async fn malformed_json_returns_a_versioned_error_envelope() {
    let response = dispatch_json("{").await;
    assert_eq!(response.protocol_version, BRIDGE_PROTOCOL_VERSION);
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "invalid_request");
}

#[tokio::test]
async fn snapshot_ids_are_validated_before_runtime_access() {
    let response =
        dispatch_json(r#"{"operation":"filesystem_snapshot_size","snapshot_id":"../escape"}"#)
            .await;
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "invalid_request");
}

#[test]
fn zero_generation_is_rejected_before_runtime_access() {
    let error = parse_generation(0).unwrap_err();
    assert_eq!(error.code, "invalid_request");
}

#[test]
fn builder_bridge_shapes_deserialize_as_typed_requests() {
    let request: BridgeRequest = serde_json::from_str(
        r#"{
            "operation":"sandbox_create",
            "image":"local/ci-base:latest",
            "mounts":[
                {"kind":"named","name":"ci-cache","target":"/cache","read_only":true}
            ],
            "network":{"mode":"bridge","name":"ci-net"},
            "ports":[{"host_port":8080,"guest_port":80}],
            "tmpfs":[{"target":"/scratch","size_bytes":4096}],
            "auto_remove":false
        }"#,
    )
    .unwrap();
    let BridgeRequest::SandboxCreate(request) = request else {
        panic!("expected sandbox builder request");
    };
    assert_eq!(
        request.mounts,
        [BridgeVolumeMount::Named {
            name: "ci-cache".to_string(),
            target: "/cache".to_string(),
            read_only: true,
        }]
    );
    assert_eq!(
        request.network,
        BridgeSandboxNetwork::Bridge {
            name: "ci-net".to_string()
        }
    );
    assert_eq!(
        request.ports,
        [BridgePortMapping {
            host_port: 8080,
            guest_port: 80
        }]
    );
    assert_eq!(request.tmpfs[0].size_bytes, Some(4096));
    assert!(!request.auto_remove);
}

#[test]
fn image_registry_options_deserialize_as_typed_requests() {
    let pull: BridgeRequest = serde_json::from_str(
        r#"{
            "operation":"image_pull",
            "reference":"registry.example/ci/base:latest",
            "credentials":{"username":"builder","password":"secret"},
            "signature_policy":{"mode":"cosign_key","public_key":"/keys/cosign.pub"}
        }"#,
    )
    .unwrap();
    let BridgeRequest::ImagePull {
        credentials,
        signature_policy,
        ..
    } = pull
    else {
        panic!("expected image pull request");
    };
    assert_eq!(
        credentials,
        Some(BridgeRegistryCredentials {
            username: "builder".to_string(),
            password: "secret".to_string(),
        })
    );
    assert_eq!(
        signature_policy,
        BridgeSignaturePolicy::CosignKey {
            public_key: "/keys/cosign.pub".to_string(),
        }
    );

    let push: BridgeRequest = serde_json::from_str(
        r#"{
            "operation":"image_push",
            "source":"local/ci:latest",
            "target":"registry.example/ci/app:latest",
            "registry_protocol":"http"
        }"#,
    )
    .unwrap();
    assert!(matches!(
        push,
        BridgeRequest::ImagePush {
            registry_protocol: Some(BridgeRegistryProtocol::Http),
            ..
        }
    ));
}

#[tokio::test]
async fn capabilities_publish_a_unique_exhaustive_operation_inventory() {
    let mut sorted = BRIDGE_OPERATIONS.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), BRIDGE_OPERATIONS.len());

    let response = handle_request(&A3sBoxClient::new(), BridgeRequest::SdkCapabilities).await;
    assert!(response.ok);
    let result = response.result.unwrap();
    assert_eq!(result["protocol_version"], BRIDGE_PROTOCOL_VERSION);
    assert_eq!(
        result["operations"],
        serde_json::to_value(BRIDGE_OPERATIONS).unwrap()
    );
    let checked_inventory: Vec<String> =
        serde_json::from_str(include_str!("../../../../sdk/bridge-operations.json")).unwrap();
    assert_eq!(checked_inventory, BRIDGE_OPERATIONS);
}

#[tokio::test]
async fn image_management_bridge_validates_before_registry_or_store_mutation() {
    let home = tempfile::tempdir().unwrap();
    let client = A3sBoxClient::from_home(home.path());

    let get = handle_request(
        &client,
        BridgeRequest::ImageGet {
            reference: "alpine:latest".to_string(),
        },
    )
    .await;
    assert!(get.ok);
    assert!(get.result.unwrap()["image"].is_null());

    let inspect = handle_request(
        &client,
        BridgeRequest::ImageInspect {
            reference: "alpine:latest".to_string(),
        },
    )
    .await;
    assert!(inspect.ok);
    assert!(inspect.result.unwrap()["image"].is_null());

    let history = handle_request(
        &client,
        BridgeRequest::ImageHistory {
            reference: "alpine:latest".to_string(),
        },
    )
    .await;
    assert!(history.ok);
    assert!(history.result.unwrap()["history"].is_null());

    let pull = handle_request(
        &client,
        BridgeRequest::ImagePull {
            reference: "alpine:latest".to_string(),
            force: false,
            platform: None,
            credentials: Some(BridgeRegistryCredentials {
                username: String::new(),
                password: "secret".to_string(),
            }),
            signature_policy: BridgeSignaturePolicy::Skip,
        },
    )
    .await;
    assert!(!pull.ok);
    assert_eq!(pull.error.unwrap().code, "invalid_request");

    let push = handle_request(
        &client,
        BridgeRequest::ImagePush {
            source: String::new(),
            target: "registry.example/ci/app:latest".to_string(),
            credentials: None,
            registry_protocol: Some(BridgeRegistryProtocol::Https),
        },
    )
    .await;
    assert!(!push.ok);
    assert_eq!(push.error.unwrap().code, "invalid_request");

    let tag = handle_request(
        &client,
        BridgeRequest::ImageTag {
            source: "missing:latest".to_string(),
            target: "local/test:latest".to_string(),
        },
    )
    .await;
    assert!(!tag.ok);
    assert_eq!(tag.error.unwrap().code, "invalid_request");
}

#[tokio::test]
async fn resource_bridge_operations_use_typed_runtime_stores() {
    let home = tempfile::tempdir().unwrap();
    let client = A3sBoxClient::from_home(home.path());

    let volume = handle_request(
        &client,
        BridgeRequest::VolumeCreate {
            name: "ci-cache".to_string(),
            labels: BTreeMap::from([("purpose".to_string(), "ci".to_string())]),
            size_limit: 4096,
        },
    )
    .await;
    assert!(volume.ok);
    assert_eq!(
        volume.result.unwrap()["mount_point"],
        home.path()
            .join("volumes/ci-cache")
            .to_string_lossy()
            .as_ref()
    );

    let network = handle_request(
        &client,
        BridgeRequest::NetworkCreate {
            name: "ci-net".to_string(),
            subnet: "10.89.88.0/24".to_string(),
            labels: BTreeMap::new(),
        },
    )
    .await;
    assert!(network.ok);
    assert_eq!(network.result.unwrap()["subnet"], "10.89.88.0/24");

    let volumes = handle_request(&client, BridgeRequest::VolumeList).await;
    assert_eq!(volumes.result.unwrap()["volumes"][0]["name"], "ci-cache");
    let networks = handle_request(&client, BridgeRequest::NetworkList).await;
    assert_eq!(networks.result.unwrap()["networks"][0]["name"], "ci-net");

    let pruned_volumes = handle_request(&client, BridgeRequest::VolumePrune).await;
    assert_eq!(pruned_volumes.result.unwrap()["names"][0], "ci-cache");
    let pruned_networks = handle_request(&client, BridgeRequest::NetworkPrune).await;
    assert_eq!(pruned_networks.result.unwrap()["names"][0], "ci-net");
}

#[tokio::test]
async fn bridge_rejects_human_image_progress_on_machine_stdout() {
    let home = tempfile::tempdir().unwrap();
    let client = A3sBoxClient::from_home(home.path());
    let response = handle_request(
        &client,
        BridgeRequest::ImageBuild {
            context_dir: ".".to_string(),
            dockerfile: None,
            tag: None,
            build_args: BTreeMap::new(),
            quiet: false,
            platforms: Vec::new(),
            target: None,
            no_cache: false,
        },
    )
    .await;
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "invalid_request");
}
