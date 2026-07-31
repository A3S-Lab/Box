//! Deterministic OCI index assembly from recorded native build outputs.

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use a3s_box_core::error::{BoxError, Result as BoxResult};
use a3s_box_core::platform::Platform;
use oci_spec::image::{
    Arch, DescriptorBuilder, ImageIndexBuilder, MediaType, Os, PlatformBuilder, Sha256Digest,
    SCHEMA_VERSION,
};
use thiserror::Error;

use super::layer::sha256_bytes;
use super::output::publish_multi_platform_build_output;
use super::{
    BoxBuildPlan, BoxBuildPlanError, BuildOperationIdentity, BuildOutputReceipt, BuildReceiptError,
    MultiPlatformBuildResult,
};
use crate::oci::image::{
    canonical_sha256_digest_hex, open_regular_file_no_follow, validate_plain_directory,
};
use crate::oci::ImageStore;

const MAX_ASSEMBLY_INPUTS: usize = 8;
const MAX_REFERENCE_BYTES: usize = 4096;

/// One exact single-platform plan and its durable recorded output.
#[derive(Debug, Clone)]
pub struct BuildOutputAssemblyInput {
    plan: BoxBuildPlan,
    receipt: BuildOutputReceipt,
}

impl BuildOutputAssemblyInput {
    /// Bind one immutable plan to the receipt that claims its output.
    pub const fn new(plan: BoxBuildPlan, receipt: BuildOutputReceipt) -> Self {
        Self { plan, receipt }
    }

    /// Exact single-platform build plan.
    pub const fn plan(&self) -> &BoxBuildPlan {
        &self.plan
    }

    /// Durable single-platform output receipt.
    pub const fn receipt(&self) -> &BuildOutputReceipt {
        &self.receipt
    }
}

/// Canonical, stateless request to assemble recorded outputs into one index.
///
/// This value owns no execution, cache, queue, journal, or publication state.
/// Construction proves that all inputs describe the same build intent and
/// source, differing only by their unique target platform.
#[derive(Debug, Clone)]
pub struct BuildOutputAssembly {
    reference: String,
    source_digest: String,
    inputs: Vec<BuildOutputAssemblyInput>,
}

impl BuildOutputAssembly {
    /// Validate and canonically sort one bounded multi-platform assembly.
    pub fn new(
        reference: impl Into<String>,
        source_digest: impl Into<String>,
        mut inputs: Vec<BuildOutputAssemblyInput>,
    ) -> Result<Self, BuildAssemblyError> {
        let reference = reference.into();
        validate_reference(&reference)?;
        let source_digest = source_digest.into();
        canonical_sha256_digest_hex(&source_digest)
            .map_err(|_| BuildAssemblyError::invalid("source digest must be canonical SHA-256"))?;
        if !(2..=MAX_ASSEMBLY_INPUTS).contains(&inputs.len()) {
            return Err(BuildAssemblyError::invalid(
                "assembly requires between two and eight recorded platforms",
            ));
        }
        inputs.sort_by(|left, right| {
            left.plan
                .platform()
                .to_string()
                .cmp(&right.plan.platform().to_string())
        });

        let baseline = inputs
            .first()
            .ok_or_else(|| BuildAssemblyError::invalid("assembly omitted its inputs"))?;
        for pair in inputs.windows(2) {
            if pair[0].plan.platform() == pair[1].plan.platform() {
                return Err(BuildAssemblyError::invalid(
                    "assembly platforms must be unique",
                ));
            }
        }
        for input in &inputs {
            if !baseline.plan.has_same_non_platform_intent(&input.plan) {
                return Err(BuildAssemblyError::invalid(
                    "assembly plans must have identical non-platform build intent",
                ));
            }
            let plan_digest = input.plan.canonical_digest()?;
            if input.receipt.source_digest != source_digest {
                return Err(BuildAssemblyError::invalid(
                    "assembly receipt source differs from the admitted source",
                ));
            }
            if input.receipt.plan_digest != plan_digest
                || input.receipt.output.platform != *input.plan.platform()
            {
                return Err(BuildAssemblyError::invalid(
                    "assembly receipt does not match its exact single-platform plan",
                ));
            }
            if input.receipt.output.reference == reference {
                return Err(BuildAssemblyError::invalid(
                    "assembly target cannot replace an input receipt reference",
                ));
            }
            let identity = BuildOperationIdentity::new(
                input.receipt.operation_id.clone(),
                source_digest.clone(),
            )?;
            input
                .receipt
                .require_identity(&identity, &plan_digest, input.plan.cache())?;
        }

        Ok(Self {
            reference,
            source_digest,
            inputs,
        })
    }

    /// Destination reference in the one Box image store.
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// Immutable source Artifact digest shared by every input.
    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }

    /// Canonically platform-sorted plan and receipt inputs.
    pub fn inputs(&self) -> &[BuildOutputAssemblyInput] {
        &self.inputs
    }

    fn platforms(&self) -> Vec<Platform> {
        self.inputs
            .iter()
            .map(|input| input.plan.platform().clone())
            .collect()
    }
}

/// Stable validation and publication failures for OCI index assembly.
#[derive(Debug, Error)]
pub enum BuildAssemblyError {
    /// The stateless assembly contract rejected inconsistent input.
    #[error("Box build output assembly is invalid: {message}")]
    Invalid { message: String },
    /// One canonical single-platform plan could not be reconstructed.
    #[error(transparent)]
    Plan(#[from] BoxBuildPlanError),
    /// One durable receipt or its ImageStore output failed revalidation.
    #[error(transparent)]
    Receipt(#[from] BuildReceiptError),
    /// Layout staging or the sole ImageStore publication boundary failed.
    #[error(transparent)]
    Build(#[from] BoxError),
}

impl BuildAssemblyError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid {
            message: message.into(),
        }
    }
}

/// Assemble already recorded single-platform outputs into one deterministic
/// OCI image index and publish it through the existing [`ImageStore`].
///
/// Every receipt is completely revalidated before staging begins. The staged
/// graph then passes the same native output validator as a direct build before
/// the sole ImageStore commit boundary is entered.
pub async fn assemble_recorded_build_outputs(
    assembly: &BuildOutputAssembly,
    store: Arc<ImageStore>,
) -> Result<MultiPlatformBuildResult, BuildAssemblyError> {
    let resolved = resolve_assembly_inputs(assembly, &store).await?;
    let staged = tokio::task::spawn_blocking(move || stage_assembly(resolved))
        .await
        .map_err(|error| {
            BoxError::BuildError(format!("OCI index assembly task failed: {error}"))
        })??;

    // Close the validation-to-copy gap against concurrent ImageStore
    // tampering or removal. Changes after this pass cannot alter the staged
    // copy, which is independently validated before publication.
    let _ = resolve_assembly_inputs(assembly, &store).await?;
    let platforms = assembly.platforms();
    publish_multi_platform_build_output(
        assembly.reference(),
        &staged.digest,
        staged.directory.path(),
        &store,
        &platforms,
    )
    .await
    .map_err(BuildAssemblyError::from)
}

async fn resolve_assembly_inputs(
    assembly: &BuildOutputAssembly,
    store: &ImageStore,
) -> Result<Vec<ResolvedAssemblyInput>, BuildAssemblyError> {
    let mut resolved = Vec::with_capacity(assembly.inputs.len());
    for input in &assembly.inputs {
        let output = input.receipt.resolve(store).await?;
        if output.platform != *input.plan.platform() {
            return Err(BuildAssemblyError::invalid(
                "revalidated output platform differs from its assembly plan",
            ));
        }
        resolved.push(ResolvedAssemblyInput {
            platform: output.platform,
            descriptor: output.descriptor,
            layout_directory: output.layout_directory,
        });
    }
    Ok(resolved)
}

struct ResolvedAssemblyInput {
    platform: Platform,
    descriptor: super::BuildOutputDescriptor,
    layout_directory: PathBuf,
}

struct StagedAssembly {
    directory: tempfile::TempDir,
    digest: String,
}

fn stage_assembly(inputs: Vec<ResolvedAssemblyInput>) -> BoxResult<StagedAssembly> {
    let directory = tempfile::Builder::new()
        .prefix("a3s-box-build-index-")
        .tempdir()
        .map_err(|error| {
            BoxError::BuildError(format!(
                "failed to create OCI index staging directory: {error}"
            ))
        })?;
    let blob_root = directory.path().join("blobs").join("sha256");
    std::fs::create_dir_all(&blob_root).map_err(|error| {
        BoxError::BuildError(format!(
            "failed to create OCI index blob directory: {error}"
        ))
    })?;

    let mut manifests = Vec::with_capacity(inputs.len());
    for input in inputs {
        copy_recorded_build_blobs(&input.layout_directory, &blob_root)?;
        let digest_hex = canonical_sha256_digest_hex(&input.descriptor.digest)?;
        let mut platform = PlatformBuilder::default()
            .architecture(Arch::from(input.platform.architecture.as_str()))
            .os(Os::from(input.platform.os.as_str()));
        if let Some(variant) = input.platform.variant {
            platform = platform.variant(variant);
        }
        manifests.push(
            DescriptorBuilder::default()
                .media_type(MediaType::ImageManifest)
                .digest(parse_digest(digest_hex)?)
                .size(input.descriptor.size)
                .platform(platform.build().map_err(|error| {
                    BoxError::BuildError(format!("invalid assembly platform: {error}"))
                })?)
                .build()
                .map_err(|error| {
                    BoxError::BuildError(format!("invalid assembly manifest descriptor: {error}"))
                })?,
        );
    }

    let image_index = ImageIndexBuilder::default()
        .schema_version(SCHEMA_VERSION)
        .media_type(MediaType::ImageIndex)
        .manifests(manifests)
        .build()
        .map_err(|error| {
            BoxError::BuildError(format!(
                "failed to build multi-platform image index: {error}"
            ))
        })?;
    let image_index_bytes = serde_json::to_vec(&image_index).map_err(|error| {
        BoxError::BuildError(format!(
            "failed to encode multi-platform image index: {error}"
        ))
    })?;
    let image_index_hex = sha256_bytes(&image_index_bytes);
    write_new_blob(&blob_root.join(&image_index_hex), &image_index_bytes)?;
    let root_descriptor = DescriptorBuilder::default()
        .media_type(MediaType::ImageIndex)
        .digest(parse_digest(&image_index_hex)?)
        .size(image_index_bytes.len() as u64)
        .build()
        .map_err(|error| {
            BoxError::BuildError(format!("invalid multi-platform root descriptor: {error}"))
        })?;
    let layout_index = ImageIndexBuilder::default()
        .schema_version(SCHEMA_VERSION)
        .media_type(MediaType::ImageIndex)
        .manifests(vec![root_descriptor])
        .build()
        .map_err(|error| {
            BoxError::BuildError(format!("failed to build OCI layout index: {error}"))
        })?;
    std::fs::write(
        directory.path().join("index.json"),
        serde_json::to_vec(&layout_index).map_err(|error| {
            BoxError::BuildError(format!("failed to encode OCI layout index: {error}"))
        })?,
    )
    .map_err(|error| BoxError::BuildError(format!("failed to write OCI layout index: {error}")))?;
    std::fs::write(
        directory.path().join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .map_err(|error| BoxError::BuildError(format!("failed to write OCI layout marker: {error}")))?;

    Ok(StagedAssembly {
        directory,
        digest: format!("sha256:{image_index_hex}"),
    })
}

fn copy_recorded_build_blobs(source_layout: &Path, target: &Path) -> BoxResult<()> {
    let source = source_layout.join("blobs").join("sha256");
    validate_plain_directory(&source, "recorded build sha256 blobs")?;
    for entry in std::fs::read_dir(&source).map_err(|error| {
        BoxError::BuildError(format!(
            "failed to inspect recorded build blobs {}: {error}",
            source.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            BoxError::BuildError(format!("failed to inspect recorded build blob: {error}"))
        })?;
        let name = entry.file_name().into_string().map_err(|_| {
            BoxError::BuildError("recorded build blob name is not UTF-8".to_string())
        })?;
        canonical_sha256_digest_hex(&format!("sha256:{name}"))?;
        let destination = target.join(&name);
        if destination.exists() {
            continue;
        }
        let mut source_file = open_regular_file_no_follow(&entry.path(), "recorded build blob")?;
        let mut destination_file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&destination)
            .map_err(|error| {
                BoxError::BuildError(format!(
                    "failed to create assembled blob {}: {error}",
                    destination.display()
                ))
            })?;
        io::copy(&mut source_file, &mut destination_file).map_err(|error| {
            BoxError::BuildError(format!(
                "failed to copy recorded build blob {name}: {error}"
            ))
        })?;
        destination_file.sync_all().map_err(|error| {
            BoxError::BuildError(format!(
                "failed to flush assembled build blob {name}: {error}"
            ))
        })?;
    }
    Ok(())
}

fn write_new_blob(path: &Path, bytes: &[u8]) -> BoxResult<()> {
    if path.exists() {
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| {
            BoxError::BuildError(format!(
                "failed to create assembled image-index blob {}: {error}",
                path.display()
            ))
        })?;
    io::Write::write_all(&mut file, bytes).map_err(|error| {
        BoxError::BuildError(format!(
            "failed to write assembled image-index blob {}: {error}",
            path.display()
        ))
    })?;
    file.sync_all().map_err(|error| {
        BoxError::BuildError(format!(
            "failed to flush assembled image-index blob {}: {error}",
            path.display()
        ))
    })
}

fn parse_digest(hex: &str) -> BoxResult<Sha256Digest> {
    Sha256Digest::from_str(hex)
        .map_err(|error| BoxError::BuildError(format!("invalid assembly digest: {error}")))
}

fn validate_reference(reference: &str) -> Result<(), BuildAssemblyError> {
    if reference.is_empty()
        || reference.len() > MAX_REFERENCE_BYTES
        || reference.trim() != reference
        || reference.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(BuildAssemblyError::invalid(
            "destination reference is outside the closed bounds",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
