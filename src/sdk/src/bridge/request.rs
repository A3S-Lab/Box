use std::collections::BTreeMap;

use serde::Deserialize;

use crate::{RegistryCredentials, RegistryProtocol, SignaturePolicy};

use super::{default_depth, default_network_subnet, default_true, BridgeSandboxCreateRequest};

/// Operations supported by this bridge protocol implementation.
///
/// Language SDKs use `sdk_capabilities` to fail closed when paired with an
/// older runtime rather than discovering a missing operation after mutation.
pub const BRIDGE_OPERATIONS: &[&str] = &[
    "sdk_capabilities",
    "image_build",
    "image_pull",
    "image_get",
    "image_list",
    "image_inspect",
    "image_history",
    "image_tag",
    "image_push",
    "image_remove",
    "image_evict",
    "volume_create",
    "volume_get",
    "volume_list",
    "volume_remove",
    "volume_prune",
    "network_create",
    "network_get",
    "network_list",
    "network_remove",
    "network_prune",
    "sandbox_create",
    "sandbox_inspect",
    "sandbox_kill",
    "sandbox_pause",
    "sandbox_resume",
    "sandbox_snapshot_create",
    "filesystem_snapshot_size",
    "filesystem_snapshot_delete",
    "command_run",
    "file_write",
    "file_read",
    "filesystem_stat",
    "filesystem_list",
    "filesystem_make_dir",
    "filesystem_move",
    "filesystem_remove",
];

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum BridgeRequest {
    SdkCapabilities,
    ImageBuild {
        context_dir: String,
        #[serde(default)]
        dockerfile: Option<String>,
        #[serde(default)]
        tag: Option<String>,
        #[serde(default)]
        build_args: BTreeMap<String, String>,
        #[serde(default = "default_true")]
        quiet: bool,
        #[serde(default)]
        platforms: Vec<String>,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        no_cache: bool,
    },
    ImagePull {
        reference: String,
        #[serde(default)]
        force: bool,
        #[serde(default)]
        platform: Option<String>,
        #[serde(default)]
        credentials: Option<BridgeRegistryCredentials>,
        #[serde(default)]
        signature_policy: BridgeSignaturePolicy,
    },
    ImageGet {
        reference: String,
    },
    ImageList,
    ImageInspect {
        reference: String,
    },
    ImageHistory {
        reference: String,
    },
    ImageTag {
        source: String,
        target: String,
    },
    ImagePush {
        source: String,
        target: String,
        #[serde(default)]
        credentials: Option<BridgeRegistryCredentials>,
        #[serde(default)]
        registry_protocol: Option<BridgeRegistryProtocol>,
    },
    ImageRemove {
        reference: String,
    },
    ImageEvict,
    VolumeCreate {
        name: String,
        #[serde(default)]
        labels: BTreeMap<String, String>,
        #[serde(default)]
        size_limit: u64,
    },
    VolumeGet {
        name: String,
    },
    VolumeList,
    VolumeRemove {
        name: String,
        #[serde(default)]
        force: bool,
    },
    VolumePrune,
    NetworkCreate {
        name: String,
        #[serde(default = "default_network_subnet")]
        subnet: String,
        #[serde(default)]
        labels: BTreeMap<String, String>,
    },
    NetworkGet {
        name: String,
    },
    NetworkList,
    NetworkRemove {
        name: String,
    },
    NetworkPrune,
    SandboxCreate(Box<BridgeSandboxCreateRequest>),
    SandboxInspect {
        sandbox_id: String,
    },
    SandboxKill {
        sandbox_id: String,
        generation: u64,
    },
    SandboxPause {
        sandbox_id: String,
        generation: u64,
        #[serde(default = "default_true")]
        keep_memory: bool,
    },
    SandboxResume {
        sandbox_id: String,
        generation: u64,
    },
    SandboxSnapshotCreate {
        sandbox_id: String,
        generation: u64,
        snapshot_id: String,
    },
    FilesystemSnapshotSize {
        snapshot_id: String,
    },
    FilesystemSnapshotDelete {
        snapshot_id: String,
    },
    CommandRun {
        sandbox_id: String,
        generation: u64,
        argv: Vec<String>,
        #[serde(default)]
        timeout_ms: Option<u64>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        stdin_base64: Option<String>,
    },
    FileWrite {
        sandbox_id: String,
        generation: u64,
        path: String,
        data_base64: String,
        #[serde(default)]
        user: Option<String>,
    },
    FileRead {
        sandbox_id: String,
        generation: u64,
        path: String,
        #[serde(default)]
        user: Option<String>,
    },
    FilesystemStat {
        sandbox_id: String,
        generation: u64,
        path: String,
        #[serde(default)]
        user: Option<String>,
    },
    FilesystemList {
        sandbox_id: String,
        generation: u64,
        path: String,
        #[serde(default = "default_depth")]
        depth: u32,
        #[serde(default)]
        user: Option<String>,
    },
    FilesystemMakeDir {
        sandbox_id: String,
        generation: u64,
        path: String,
        #[serde(default)]
        user: Option<String>,
    },
    FilesystemMove {
        sandbox_id: String,
        generation: u64,
        path: String,
        destination: String,
        #[serde(default)]
        user: Option<String>,
    },
    FilesystemRemove {
        sandbox_id: String,
        generation: u64,
        path: String,
        #[serde(default)]
        user: Option<String>,
    },
}

impl BridgeRequest {
    pub const fn operation_name(&self) -> &'static str {
        match self {
            Self::SdkCapabilities => "sdk_capabilities",
            Self::ImageBuild { .. } => "image_build",
            Self::ImagePull { .. } => "image_pull",
            Self::ImageGet { .. } => "image_get",
            Self::ImageList => "image_list",
            Self::ImageInspect { .. } => "image_inspect",
            Self::ImageHistory { .. } => "image_history",
            Self::ImageTag { .. } => "image_tag",
            Self::ImagePush { .. } => "image_push",
            Self::ImageRemove { .. } => "image_remove",
            Self::ImageEvict => "image_evict",
            Self::VolumeCreate { .. } => "volume_create",
            Self::VolumeGet { .. } => "volume_get",
            Self::VolumeList => "volume_list",
            Self::VolumeRemove { .. } => "volume_remove",
            Self::VolumePrune => "volume_prune",
            Self::NetworkCreate { .. } => "network_create",
            Self::NetworkGet { .. } => "network_get",
            Self::NetworkList => "network_list",
            Self::NetworkRemove { .. } => "network_remove",
            Self::NetworkPrune => "network_prune",
            Self::SandboxCreate(_) => "sandbox_create",
            Self::SandboxInspect { .. } => "sandbox_inspect",
            Self::SandboxKill { .. } => "sandbox_kill",
            Self::SandboxPause { .. } => "sandbox_pause",
            Self::SandboxResume { .. } => "sandbox_resume",
            Self::SandboxSnapshotCreate { .. } => "sandbox_snapshot_create",
            Self::FilesystemSnapshotSize { .. } => "filesystem_snapshot_size",
            Self::FilesystemSnapshotDelete { .. } => "filesystem_snapshot_delete",
            Self::CommandRun { .. } => "command_run",
            Self::FileWrite { .. } => "file_write",
            Self::FileRead { .. } => "file_read",
            Self::FilesystemStat { .. } => "filesystem_stat",
            Self::FilesystemList { .. } => "filesystem_list",
            Self::FilesystemMakeDir { .. } => "filesystem_make_dir",
            Self::FilesystemMove { .. } => "filesystem_move",
            Self::FilesystemRemove { .. } => "filesystem_remove",
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct BridgeRegistryCredentials {
    pub(super) username: String,
    pub(super) password: String,
}

impl From<BridgeRegistryCredentials> for RegistryCredentials {
    fn from(value: BridgeRegistryCredentials) -> Self {
        Self::basic(value.username, value.password)
    }
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum BridgeSignaturePolicy {
    #[default]
    Skip,
    CosignKey {
        public_key: String,
    },
    CosignKeyless {
        issuer: String,
        identity: String,
    },
}

impl From<BridgeSignaturePolicy> for SignaturePolicy {
    fn from(value: BridgeSignaturePolicy) -> Self {
        match value {
            BridgeSignaturePolicy::Skip => Self::Skip,
            BridgeSignaturePolicy::CosignKey { public_key } => Self::CosignKey { public_key },
            BridgeSignaturePolicy::CosignKeyless { issuer, identity } => {
                Self::CosignKeyless { issuer, identity }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BridgeRegistryProtocol {
    Https,
    Http,
}

impl From<BridgeRegistryProtocol> for RegistryProtocol {
    fn from(value: BridgeRegistryProtocol) -> Self {
        match value {
            BridgeRegistryProtocol::Https => Self::Https,
            BridgeRegistryProtocol::Http => Self::Http,
        }
    }
}
