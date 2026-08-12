//! Immutable, versioned evidence for one policy-governed execution generation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::ExecutionIsolation;
use crate::error::{BoxError, Result};
use crate::execution::{ExecutionBackend, IsolationClass};
use crate::host_mount_policy::ResolvedHostMount;
use crate::security_policy::EgressPolicy;
use crate::traits::ExecutionGeneration;

/// Stable schema identifier for the first security receipt format.
pub const SECURITY_RECEIPT_V1_SCHEMA: &str = "a3s.box.security-receipt.v1";

/// A sealed receipt. `digest` covers the canonical JSON representation of
/// `evidence`; it does not cover itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityReceiptV1 {
    pub schema: String,
    pub digest: String,
    pub evidence: SecurityReceiptEvidenceV1,
}

/// Evidence assembled after backend preparation and before the launch call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityReceiptEvidenceV1 {
    pub execution_id: String,
    pub generation: ExecutionGeneration,
    pub request_digest: String,
    pub policy_digest: String,
    pub execution_plan_digest: String,
    pub requested_isolation: ExecutionIsolation,
    pub backend: ExecutionBackend,
    pub isolation_class: IsolationClass,
    pub image: SecurityReceiptImageIdentity,
    pub artifacts: SecurityReceiptArtifactDigests,
    pub owner: SecurityReceiptOwnerIdentity,
    pub mounts: Vec<ResolvedHostMount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_egress: Option<EgressPolicy>,
    pub runtime_controls: SecurityReceiptRuntimeControls,
    pub host_capability_digest: String,
    pub preparation: SecurityReceiptPreparation,
    pub launch_timestamp: DateTime<Utc>,
}

/// Image and rootfs identity selected for the generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityReceiptImageIdentity {
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_digest: Option<String>,
    pub rootfs_digest: String,
}

/// Runtime artifacts used at the isolation boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityReceiptArtifactDigests {
    pub runtime_sha256: String,
    pub agent_sha256: String,
}

/// Local owner responsible for the runtime generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityReceiptOwnerIdentity {
    pub platform: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_gid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

/// One effective user-namespace mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityReceiptIdMapping {
    pub container_id: u32,
    pub host_id: u32,
    pub size: u32,
}

/// Runtime controls compiled for the generation. No environment or secret
/// values are represented by this schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityReceiptRuntimeControls {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uid_mappings: Vec<SecurityReceiptIdMapping>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gid_mappings: Vec<SecurityReceiptIdMapping>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped_capabilities: Vec<String>,
    pub seccomp: String,
    pub no_new_privileges: bool,
    pub resources: SecurityReceiptResources,
}

/// Effective resource values bound into the backend launch configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityReceiptResources {
    pub vcpus: u32,
    pub memory_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_shares: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_quota: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_period: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpuset_cpus: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_reservation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_swap: Option<i64>,
}

/// Backend state that was complete when the immutable receipt was published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecurityReceiptPreparation {
    ReadyToLaunch,
    ReadyToResume,
}

impl SecurityReceiptV1 {
    /// Seal one fully prepared evidence payload.
    pub fn seal(evidence: SecurityReceiptEvidenceV1) -> Result<Self> {
        validate_evidence(&evidence)?;
        let digest = canonical_json_digest(&evidence)?;
        Ok(Self {
            schema: SECURITY_RECEIPT_V1_SCHEMA.to_string(),
            digest,
            evidence,
        })
    }

    /// Validate the schema, evidence invariants, and self-digest.
    pub fn validate(&self) -> Result<()> {
        if self.schema != SECURITY_RECEIPT_V1_SCHEMA {
            return Err(receipt_error(format!(
                "unsupported schema {:?}",
                self.schema
            )));
        }
        validate_evidence(&self.evidence)?;
        let expected = canonical_json_digest(&self.evidence)?;
        if self.digest != expected {
            return Err(receipt_error("receipt digest does not match its evidence"));
        }
        Ok(())
    }
}

/// Compute a deterministic SHA-256 digest over recursively key-sorted JSON.
pub fn canonical_json_digest(value: &impl Serialize) -> Result<String> {
    let value = serde_json::to_value(value).map_err(|error| {
        BoxError::SerializationError(format!("failed to encode canonical JSON: {error}"))
    })?;
    let canonical = canonical_json(value);
    let bytes = serde_json::to_vec(&canonical).map_err(|error| {
        BoxError::SerializationError(format!("failed to serialize canonical JSON: {error}"))
    })?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

fn canonical_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonical_json).collect())
        }
        serde_json::Value::Object(values) => {
            let mut entries = values.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonical_json(value)))
                    .collect(),
            )
        }
        scalar => scalar,
    }
}

fn validate_evidence(evidence: &SecurityReceiptEvidenceV1) -> Result<()> {
    if evidence.execution_id.trim().is_empty() {
        return Err(receipt_error("execution ID cannot be empty"));
    }
    for (label, digest) in [
        ("request", evidence.request_digest.as_str()),
        ("policy", evidence.policy_digest.as_str()),
        ("execution plan", evidence.execution_plan_digest.as_str()),
        ("rootfs", evidence.image.rootfs_digest.as_str()),
        (
            "runtime artifact",
            evidence.artifacts.runtime_sha256.as_str(),
        ),
        ("agent artifact", evidence.artifacts.agent_sha256.as_str()),
        ("host capability", evidence.host_capability_digest.as_str()),
    ] {
        validate_sha256(label, digest)?;
    }
    if let Some(digest) = evidence.image.manifest_digest.as_deref() {
        validate_sha256("image manifest", digest)?;
    }
    if evidence.runtime_controls.seccomp.trim().is_empty() {
        return Err(receipt_error("seccomp posture cannot be empty"));
    }
    if evidence.runtime_controls.resources.vcpus == 0
        || evidence.runtime_controls.resources.memory_bytes == 0
    {
        return Err(receipt_error(
            "effective CPU and memory resources must be non-zero",
        ));
    }
    Ok(())
}

fn validate_sha256(label: &str, value: &str) -> Result<()> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(receipt_error(format!("{label} digest is not SHA-256")));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(receipt_error(format!(
            "{label} digest is not canonical SHA-256"
        )));
    }
    Ok(())
}

fn receipt_error(message: impl Into<String>) -> BoxError {
    BoxError::StateError(format!("security receipt: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn digest(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    fn evidence() -> SecurityReceiptEvidenceV1 {
        SecurityReceiptEvidenceV1 {
            execution_id: "execution-1".to_string(),
            generation: ExecutionGeneration::INITIAL,
            request_digest: digest('a'),
            policy_digest: digest('b'),
            execution_plan_digest: digest('c'),
            requested_isolation: ExecutionIsolation::Microvm,
            backend: ExecutionBackend::Krun,
            isolation_class: IsolationClass::HardwareVm,
            image: SecurityReceiptImageIdentity {
                reference: "alpine:3.20".to_string(),
                manifest_digest: Some(digest('d')),
                rootfs_digest: digest('e'),
            },
            artifacts: SecurityReceiptArtifactDigests {
                runtime_sha256: digest('f'),
                agent_sha256: digest('1'),
            },
            owner: SecurityReceiptOwnerIdentity {
                platform: "test".to_string(),
                effective_uid: Some(1000),
                effective_gid: Some(1000),
                username: Some("runner".to_string()),
            },
            mounts: Vec::new(),
            effective_egress: None,
            runtime_controls: SecurityReceiptRuntimeControls {
                uid_mappings: Vec::new(),
                gid_mappings: Vec::new(),
                capabilities: Vec::new(),
                dropped_capabilities: Vec::new(),
                seccomp: "default".to_string(),
                no_new_privileges: true,
                resources: SecurityReceiptResources {
                    vcpus: 1,
                    memory_bytes: 128 * 1024 * 1024,
                    pids_limit: None,
                    cpu_shares: None,
                    cpu_quota: None,
                    cpu_period: None,
                    cpuset_cpus: None,
                    memory_reservation: None,
                    memory_swap: None,
                },
            },
            host_capability_digest: digest('2'),
            preparation: SecurityReceiptPreparation::ReadyToLaunch,
            launch_timestamp: DateTime::<Utc>::UNIX_EPOCH,
        }
    }

    #[test]
    fn sealed_receipt_round_trips_and_detects_tampering() {
        let receipt = SecurityReceiptV1::seal(evidence()).unwrap();
        receipt.validate().unwrap();
        let decoded: SecurityReceiptV1 =
            serde_json::from_value(serde_json::to_value(&receipt).unwrap()).unwrap();
        assert_eq!(decoded, receipt);

        let mut tampered = receipt;
        tampered.evidence.runtime_controls.no_new_privileges = false;
        assert!(tampered.validate().is_err());
    }

    #[test]
    fn canonical_digest_sorts_nested_map_keys() {
        let mut first = HashMap::new();
        first.insert("z", serde_json::json!({"b": 2, "a": 1}));
        first.insert("a", serde_json::json!([3, 2, 1]));
        let mut second = HashMap::new();
        second.insert("a", serde_json::json!([3, 2, 1]));
        second.insert("z", serde_json::json!({"a": 1, "b": 2}));

        assert_eq!(
            canonical_json_digest(&first).unwrap(),
            canonical_json_digest(&second).unwrap()
        );
    }

    #[test]
    fn receipt_schema_contains_no_environment_or_secret_value_fields() {
        let value = serde_json::to_string(&SecurityReceiptV1::seal(evidence()).unwrap()).unwrap();
        assert!(!value.contains("environment"));
        assert!(!value.contains("secret"));
        assert!(!value.contains("authorization"));
        assert!(!value.contains("proxy_header"));
    }
}
