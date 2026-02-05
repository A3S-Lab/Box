//! TEE (Trusted Execution Environment) support.
//!
//! This module provides hardware detection and configuration for
//! Trusted Execution Environments, currently supporting AMD SEV-SNP.

pub mod snp;

pub use snp::{check_sev_snp_support, require_sev_snp_support, SevSnpSupport};
