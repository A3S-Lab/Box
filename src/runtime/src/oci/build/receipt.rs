//! Durable terminal receipts for plan-bound native OCI builds.
//!
//! Receipts bind one caller operation and immutable source digest to the exact
//! native OCI output already owned by [`ImageStore`]. They deliberately do not
//! copy image content or supervise an in-flight build. A later operation
//! supervisor can use this terminal boundary for start/inspect/cancel recovery
//! without adding another build engine or image store.

use a3s_box_core::platform::Platform;
use a3s_box_core::OperationId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::output::inspect_stored_build_output;
use super::{BuildOutputDescriptor, BuildResult, OCI_IMAGE_MANIFEST_MEDIA_TYPE};
use crate::oci::image::canonical_sha256_digest_hex;
use crate::oci::ImageStore;

const MAX_OPERATION_ID_BYTES: usize = 255;
const MAX_RECEIPT_BYTES: u64 = 64 * 1024;
const RECEIPT_DIRECTORY: &str = "build-receipts";

mod journal;

pub(super) use journal::BuildOperationJournal;

/// Immutable caller identity for one recoverable build output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOperationIdentity {
    operation_id: OperationId,
    source_digest: String,
    output_reference: String,
}

impl BuildOperationIdentity {
    /// Validate one bounded operation ID and canonical source Artifact digest.
    pub fn new(
        operation_id: OperationId,
        source_digest: impl Into<String>,
    ) -> Result<Self, BuildReceiptError> {
        if operation_id.as_str().len() > MAX_OPERATION_ID_BYTES
            || operation_id
                .as_str()
                .bytes()
                .any(|byte| byte.is_ascii_control())
        {
            return Err(BuildReceiptError::InvalidIdentity {
                field: "operation_id",
                reason: "must contain at most 255 non-control UTF-8 bytes",
            });
        }
        let source_digest = source_digest.into();
        if canonical_sha256_digest_hex(&source_digest).is_err() {
            return Err(BuildReceiptError::InvalidIdentity {
                field: "source_digest",
                reason: "must be canonical sha256:<64 lowercase hex>",
            });
        }
        let operation_key = operation_key(&operation_id);
        Ok(Self {
            operation_id,
            source_digest,
            output_reference: format!("a3s-box/build-operation:{operation_key}"),
        })
    }

    /// Caller-owned idempotency identity.
    pub fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    /// Immutable source Artifact content identity.
    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }

    /// Box-internal image reference derived from the operation identity.
    pub fn output_reference(&self) -> &str {
        &self.output_reference
    }
}

/// Intent persisted before native build side effects begin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PendingBuildOperation {
    schema: String,
    operation_id: OperationId,
    source_digest: String,
    plan_digest: String,
    output_reference: String,
}

impl PendingBuildOperation {
    const SCHEMA: &'static str = "a3s.box.build-output-intent.v1";

    pub(super) fn new(
        identity: &BuildOperationIdentity,
        plan_digest: String,
    ) -> Result<Self, BuildReceiptError> {
        let pending = Self {
            schema: Self::SCHEMA.to_string(),
            operation_id: identity.operation_id.clone(),
            source_digest: identity.source_digest.clone(),
            plan_digest,
            output_reference: identity.output_reference.clone(),
        };
        pending.require_identity(identity, &pending.plan_digest)?;
        Ok(pending)
    }

    fn validate(&self) -> Result<(), BuildReceiptError> {
        if self.schema != Self::SCHEMA
            || !valid_operation_id(&self.operation_id)
            || canonical_sha256_digest_hex(&self.source_digest).is_err()
            || canonical_sha256_digest_hex(&self.plan_digest).is_err()
            || self.output_reference
                != format!(
                    "a3s-box/build-operation:{}",
                    operation_key(&self.operation_id)
                )
        {
            return Err(BuildReceiptError::InvalidReceipt {
                operation_id: self.operation_id.to_string(),
                message: "pending build intent violates its closed identity".to_string(),
            });
        }
        Ok(())
    }

    pub(super) fn require_identity(
        &self,
        identity: &BuildOperationIdentity,
        plan_digest: &str,
    ) -> Result<(), BuildReceiptError> {
        self.validate()?;
        if self.operation_id != identity.operation_id
            || self.source_digest != identity.source_digest
            || self.plan_digest != plan_digest
            || self.output_reference != identity.output_reference
        {
            return Err(BuildReceiptError::Conflict {
                operation_id: identity.operation_id.to_string(),
                message: "the pending source, plan, or output identity differs".to_string(),
            });
        }
        Ok(())
    }

    fn matches_receipt(&self, receipt: &BuildOutputReceipt) -> bool {
        self.operation_id == receipt.operation_id
            && self.source_digest == receipt.source_digest
            && self.plan_digest == receipt.plan_digest
            && self.output_reference == receipt.output.reference
    }
}

/// Strict on-disk state for one build operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub(super) enum PersistedBuildOperation {
    Pending(PendingBuildOperation),
    Succeeded(BuildOutputReceipt),
}

impl PersistedBuildOperation {
    fn validate(&self) -> Result<(), BuildReceiptError> {
        match self {
            Self::Pending(pending) => pending.validate(),
            Self::Succeeded(receipt) => receipt.validate(),
        }
    }

    fn operation_id(&self) -> &OperationId {
        match self {
            Self::Pending(pending) => &pending.operation_id,
            Self::Succeeded(receipt) => &receipt.operation_id,
        }
    }
}

/// Persisted, path-independent description of one native OCI output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildReceiptOutput {
    /// Operation-specific internal ImageStore reference.
    pub reference: String,
    /// Exact root OCI manifest descriptor.
    pub descriptor: BuildOutputDescriptor,
    /// Exact single-platform output.
    pub platform: Platform,
    /// Bytes occupied by the durable ImageStore layout.
    pub content_bytes: u64,
    /// Manifest layer count.
    pub layer_count: u64,
    /// Content-addressed blob count.
    pub blob_count: u64,
    /// Canonical digest of the sorted digest-and-size blob inventory.
    pub blob_inventory_digest: String,
}

/// Durable terminal receipt for a successful native build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildOutputReceipt {
    /// Exact receipt schema.
    pub schema: String,
    /// Caller-owned idempotency identity.
    pub operation_id: OperationId,
    /// Immutable source Artifact content identity.
    pub source_digest: String,
    /// Canonical A3S ACL build-plan identity.
    pub plan_digest: String,
    /// Path-independent native OCI output evidence.
    pub output: BuildReceiptOutput,
}

impl BuildOutputReceipt {
    /// Current closed receipt schema.
    pub const SCHEMA: &'static str = "a3s.box.build-output-receipt.v1";

    pub(super) fn from_result(
        identity: &BuildOperationIdentity,
        plan_digest: String,
        result: &BuildResult,
    ) -> Result<Self, BuildReceiptError> {
        let layer_count =
            u64::try_from(result.layer_count).map_err(|_| BuildReceiptError::OutputInvalid {
                operation_id: identity.operation_id.to_string(),
                message: "layer count exceeds the durable receipt range".to_string(),
            })?;
        let blob_count =
            u64::try_from(result.blob_count).map_err(|_| BuildReceiptError::OutputInvalid {
                operation_id: identity.operation_id.to_string(),
                message: "blob count exceeds the durable receipt range".to_string(),
            })?;
        let receipt = Self {
            schema: Self::SCHEMA.to_string(),
            operation_id: identity.operation_id.clone(),
            source_digest: identity.source_digest.clone(),
            plan_digest,
            output: BuildReceiptOutput {
                reference: result.reference.clone(),
                descriptor: result.descriptor.clone(),
                platform: result.platform.clone(),
                content_bytes: result.content_bytes(),
                layer_count,
                blob_count,
                blob_inventory_digest: result.blob_inventory_digest.clone(),
            },
        };
        receipt.validate()?;
        receipt.require_identity(identity, &receipt.plan_digest)?;
        Ok(receipt)
    }

    fn validate(&self) -> Result<(), BuildReceiptError> {
        let operation_id = self.operation_id.to_string();
        if self.schema != Self::SCHEMA {
            return Err(BuildReceiptError::InvalidReceipt {
                operation_id,
                message: format!("unsupported schema {:?}", self.schema),
            });
        }
        if !valid_operation_id(&self.operation_id) {
            return Err(BuildReceiptError::InvalidReceipt {
                operation_id,
                message: "operation ID is outside the closed receipt bounds".to_string(),
            });
        }
        for (field, digest) in [
            ("source digest", self.source_digest.as_str()),
            ("plan digest", self.plan_digest.as_str()),
            ("output digest", self.output.descriptor.digest.as_str()),
            (
                "blob inventory digest",
                self.output.blob_inventory_digest.as_str(),
            ),
        ] {
            if canonical_sha256_digest_hex(digest).is_err() {
                return Err(BuildReceiptError::InvalidReceipt {
                    operation_id,
                    message: format!("{field} is not canonical SHA-256"),
                });
            }
        }
        if self.output.reference
            != format!(
                "a3s-box/build-operation:{}",
                operation_key(&self.operation_id)
            )
        {
            return Err(BuildReceiptError::InvalidReceipt {
                operation_id,
                message: "output reference is not derived from the operation identity".to_string(),
            });
        }
        if self.output.descriptor.media_type != OCI_IMAGE_MANIFEST_MEDIA_TYPE
            || self.output.descriptor.size == 0
            || self.output.content_bytes < self.output.descriptor.size
            || self.output.blob_count < 2
        {
            return Err(BuildReceiptError::InvalidReceipt {
                operation_id,
                message: "output descriptor or content counts are invalid".to_string(),
            });
        }
        if self.output.platform.os != "linux" || self.output.platform.architecture.trim().is_empty()
        {
            return Err(BuildReceiptError::InvalidReceipt {
                operation_id,
                message: "output platform is outside the native build contract".to_string(),
            });
        }
        Ok(())
    }

    pub(super) fn require_identity(
        &self,
        identity: &BuildOperationIdentity,
        plan_digest: &str,
    ) -> Result<(), BuildReceiptError> {
        self.validate()?;
        if self.operation_id != identity.operation_id
            || self.source_digest != identity.source_digest
            || self.plan_digest != plan_digest
            || self.output.reference != identity.output_reference
        {
            return Err(BuildReceiptError::Conflict {
                operation_id: identity.operation_id.to_string(),
                message: "the persisted source, plan, or output identity differs".to_string(),
            });
        }
        Ok(())
    }

    pub(super) async fn resolve(
        &self,
        store: &ImageStore,
    ) -> Result<BuildResult, BuildReceiptError> {
        self.validate()?;
        let actual = inspect_stored_output(&self.operation_id, &self.output.reference, store)
            .await?
            .ok_or_else(|| BuildReceiptError::OutputMissing {
                operation_id: self.operation_id.to_string(),
                reference: self.output.reference.clone(),
            })?;
        if actual.descriptor != self.output.descriptor
            || actual.platform != self.output.platform
            || actual.content_bytes() != self.output.content_bytes
            || u64::try_from(actual.layer_count).ok() != Some(self.output.layer_count)
            || u64::try_from(actual.blob_count).ok() != Some(self.output.blob_count)
            || actual.blob_inventory_digest != self.output.blob_inventory_digest
        {
            return Err(BuildReceiptError::OutputInvalid {
                operation_id: self.operation_id.to_string(),
                message: "revalidated ImageStore output differs from the receipt".to_string(),
            });
        }
        Ok(actual)
    }
}

pub(super) async fn inspect_stored_output(
    operation_id: &OperationId,
    reference: &str,
    store: &ImageStore,
) -> Result<Option<BuildResult>, BuildReceiptError> {
    let Some(stored) =
        store
            .get_checked(reference)
            .await
            .map_err(|error| BuildReceiptError::OutputInvalid {
                operation_id: operation_id.to_string(),
                message: format!("failed to read the authoritative ImageStore index: {error}"),
            })?
    else {
        return Ok(None);
    };
    let reference = reference.to_string();
    let store_root = store.store_dir().to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        inspect_stored_build_output(&reference, stored, &store_root)
    })
    .await
    .map_err(|error| BuildReceiptError::Task {
        operation_id: operation_id.to_string(),
        message: format!("OCI receipt validation task failed: {error}"),
    })?
    .map_err(|error| BuildReceiptError::OutputInvalid {
        operation_id: operation_id.to_string(),
        message: error.to_string(),
    })?;
    Ok(Some(output))
}

/// A successful recorded execution or exact durable replay.
#[derive(Debug)]
pub struct RecordedBuildResult {
    /// Stable path-independent terminal receipt.
    pub receipt: BuildOutputReceipt,
    /// Revalidated store-owned OCI output.
    pub output: BuildResult,
    /// Whether this call replayed an existing terminal receipt.
    pub replayed: bool,
}

/// Fail-closed receipt identity, persistence, conflict, and output errors.
#[derive(Debug, Error)]
pub enum BuildReceiptError {
    /// Caller identity is outside the closed contract.
    #[error("Box build receipt identity field {field} {reason}")]
    InvalidIdentity {
        field: &'static str,
        reason: &'static str,
    },
    /// Receipt storage could not be used safely.
    #[error("Box build receipt store is unsafe: {message}")]
    UnsafeStore { message: String },
    /// Receipt persistence failed.
    #[error("Box build receipt store I/O failed: {message}: {source}")]
    StoreIo {
        message: String,
        #[source]
        source: std::io::Error,
    },
    /// Blocking persistence task failed.
    #[error("Box build receipt task failed for {operation_id}: {message}")]
    Task {
        operation_id: String,
        message: String,
    },
    /// Existing receipt bytes or fields violate the closed schema.
    #[error("Box build receipt is invalid for {operation_id}: {message}")]
    InvalidReceipt {
        operation_id: String,
        message: String,
    },
    /// One operation ID was reused for different immutable intent.
    #[error("Box build receipt conflict for {operation_id}: {message}")]
    Conflict {
        operation_id: String,
        message: String,
    },
    /// Receipt exists but its ImageStore reference is absent.
    #[error("Box build output is missing for {operation_id}: ImageStore reference {reference}")]
    OutputMissing {
        operation_id: String,
        reference: String,
    },
    /// Persisted output no longer proves the receipt.
    #[error("Box build output is invalid for {operation_id}: {message}")]
    OutputInvalid {
        operation_id: String,
        message: String,
    },
}

fn operation_key(operation_id: &OperationId) -> String {
    format!("{:x}", Sha256::digest(operation_id.as_str().as_bytes()))
}

fn valid_operation_id(operation_id: &OperationId) -> bool {
    operation_id.as_str().len() <= MAX_OPERATION_ID_BYTES
        && !operation_id
            .as_str()
            .bytes()
            .any(|byte| byte.is_ascii_control())
}

#[cfg(test)]
mod tests;
