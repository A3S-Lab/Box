//! Durable identity and receipt types for one native cache export.

use std::path::PathBuf;

use a3s_box_core::error::Result;
use a3s_box_core::platform::Platform;
use oci_spec::image::MediaType;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{cache_error, CACHE_CONFIG_SCHEMA, MAX_CACHE_ENTRIES};
use crate::oci::build::BuildOutputDescriptor;
use crate::oci::image::canonical_sha256_digest_hex;

const CACHE_KEY_PROFILE: &str = "a3s.box.build-cache-key.v1";

/// Path-independent evidence for one portable native cache artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildCacheReceipt {
    pub schema: String,
    pub key: String,
    pub source_digest: String,
    pub plan_digest: String,
    pub descriptor: BuildOutputDescriptor,
    pub platform: Platform,
    pub content_bytes: u64,
    pub entry_count: u64,
    pub blob_count: u64,
    pub blob_inventory_digest: String,
}

impl BuildCacheReceipt {
    pub const SCHEMA: &'static str = "a3s.box.build-cache-receipt.v1";

    pub(in crate::oci::build) fn validate(&self) -> Result<()> {
        if self.schema != Self::SCHEMA
            || canonical_sha256_digest_hex(&self.key).is_err()
            || canonical_sha256_digest_hex(&self.source_digest).is_err()
            || canonical_sha256_digest_hex(&self.plan_digest).is_err()
            || canonical_sha256_digest_hex(&self.descriptor.digest).is_err()
            || canonical_sha256_digest_hex(&self.blob_inventory_digest).is_err()
            || self.descriptor.media_type != MediaType::ImageManifest.as_ref()
            || self.descriptor.size == 0
            || self.content_bytes < self.descriptor.size
            || self.entry_count > MAX_CACHE_ENTRIES as u64
            || self.blob_count < 2
            || self.platform.os != "linux"
            || self.platform.architecture.trim().is_empty()
            || self.key != cache_key(&self.source_digest, &self.plan_digest, &self.platform)?
        {
            return Err(cache_error(
                "native build cache receipt violates its closed identity",
            ));
        }
        Ok(())
    }
}

/// Revalidated portable cache artifact owned by a recorded operation.
#[derive(Debug)]
pub struct RecordedBuildCache {
    pub receipt: BuildCacheReceipt,
    pub layout_directory: PathBuf,
}

/// Immutable identity compiled into the cache artifact itself.
#[derive(Debug, Clone)]
pub(in crate::oci::build) struct BuildCacheExportIdentity {
    pub(super) source_digest: String,
    pub(super) plan_digest: String,
    pub(super) platform: Platform,
    pub(super) key: String,
}

impl BuildCacheExportIdentity {
    pub(in crate::oci::build) fn new(
        source_digest: impl Into<String>,
        plan_digest: impl Into<String>,
        platform: Platform,
    ) -> Result<Self> {
        let source_digest = source_digest.into();
        let plan_digest = plan_digest.into();
        canonical_sha256_digest_hex(&source_digest)?;
        canonical_sha256_digest_hex(&plan_digest)?;
        if platform.os != "linux" || platform.architecture.trim().is_empty() {
            return Err(cache_error("native build cache platform is invalid"));
        }
        let key = cache_key(&source_digest, &plan_digest, &platform)?;
        Ok(Self {
            source_digest,
            plan_digest,
            platform,
            key,
        })
    }
}

fn cache_key(source_digest: &str, plan_digest: &str, platform: &Platform) -> Result<String> {
    let mut digest = Sha256::new();
    for value in [
        CACHE_KEY_PROFILE,
        CACHE_CONFIG_SCHEMA,
        source_digest,
        plan_digest,
        platform.os.as_str(),
        platform.architecture.as_str(),
        platform.variant.as_deref().unwrap_or(""),
    ] {
        let length = u64::try_from(value.len())
            .map_err(|_| cache_error("native cache key field exceeds its bound"))?;
        digest.update(length.to_be_bytes());
        digest.update(value.as_bytes());
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}
