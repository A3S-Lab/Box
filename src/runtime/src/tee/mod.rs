//! TEE (Trusted Execution Environment) support.
//!
//! This module provides hardware detection, configuration, attestation,
//! and verification for Trusted Execution Environments (AMD SEV-SNP).
//!
//! - `snp`: Hardware detection for AMD SEV-SNP.
//! - `attestation`: Attestation report types and parsing.
//! - `verifier`: Host-side report verification (signature + policy).
//! - `policy`: Verification policy definitions.
//! - `certs`: AMD KDS certificate fetching and caching.

pub mod attestation;
pub mod certs;
pub mod policy;
pub mod simulate;
pub mod snp;
pub mod verifier;

pub use attestation::{
    parse_platform_info, AttestationReport, AttestationRequest, CertificateChain, PlatformInfo,
    TcbVersion,
};
pub use certs::AmdKdsClient;
pub use policy::{AttestationPolicy, MinTcbPolicy, PolicyResult, PolicyViolation};
pub use snp::{check_sev_snp_support, require_sev_snp_support, SevSnpSupport};
pub use verifier::{verify_attestation, VerificationResult};
pub use simulate::{
    build_simulated_report, is_simulate_mode, is_simulated_report, TEE_SIMULATE_ENV,
};
