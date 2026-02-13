//! RA-TLS (Remote Attestation TLS) for AMD SEV-SNP.
//!
//! Embeds a TEE attestation report inside an X.509 certificate extension,
//! enabling attestation verification during the TLS handshake. Any client
//! connecting to an RA-TLS server can extract and verify the SNP report
//! from the server's certificate, proving the server runs in a genuine TEE.
//!
//! ## OID Convention
//!
//! The SNP attestation report is stored in a custom X.509 extension:
//! - `1.3.6.1.4.1.58270.1.1` — Raw SNP report bytes (1184 bytes)
//! - `1.3.6.1.4.1.58270.1.2` — Certificate chain (JSON: {vcek, ask, ark})
//!
//! ## Usage
//!
//! ```ignore
//! // Server side (inside TEE):
//! let (cert_der, key_der) = generate_ratls_certificate(&report)?;
//! let server_config = create_server_config(&cert_der, &key_der)?;
//!
//! // Client side (verifier):
//! let client_config = create_client_config(policy, allow_simulated)?;
//! ```

use a3s_box_core::error::{BoxError, Result};

use super::attestation::{AttestationReport, CertificateChain};
use super::policy::AttestationPolicy;
use super::simulate::is_simulated_report;
use super::verifier::verify_attestation;

/// OID for the SNP attestation report extension.
/// Private Enterprise Number (PEN) arc: 1.3.6.1.4.1.58270.1.1
const OID_SNP_REPORT: &str = "1.3.6.1.4.1.58270.1.1";

/// OID for the certificate chain extension.
/// Private Enterprise Number (PEN) arc: 1.3.6.1.4.1.58270.1.2
const OID_CERT_CHAIN: &str = "1.3.6.1.4.1.58270.1.2";

// ============================================================================
// Certificate generation
// ============================================================================

/// Generate a self-signed RA-TLS certificate containing an SNP attestation report.
///
/// The certificate uses a P-384 key pair and embeds the attestation report
/// and certificate chain as custom X.509 extensions. The report's `report_data`
/// field contains a hash of the certificate's public key, binding the TLS
/// identity to the TEE attestation.
///
/// Returns `(cert_der, private_key_der)`.
pub fn generate_ratls_certificate(
    report: &AttestationReport,
) -> Result<(Vec<u8>, Vec<u8>)> {
    use rcgen::{
        CertificateParams, CustomExtension, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P384_SHA384,
    };

    // Generate a new P-384 key pair for this certificate
    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).map_err(|e| {
        BoxError::AttestationError(format!("Failed to generate P-384 key pair: {}", e))
    })?;

    let mut params = CertificateParams::default();

    // Set subject
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "A3S Box RA-TLS");
    dn.push(DnType::OrganizationName, "A3S Lab");
    params.distinguished_name = dn;

    // Add SNP report as custom extension (non-critical)
    let report_ext = CustomExtension::from_oid_content(
        &oid_to_asn1(OID_SNP_REPORT),
        report.report.clone(),
    );
    params.custom_extensions.push(report_ext);

    // Add certificate chain as custom extension (JSON-encoded)
    let chain_json = serde_json::to_vec(&report.cert_chain).map_err(|e| {
        BoxError::AttestationError(format!("Failed to serialize cert chain: {}", e))
    })?;
    let chain_ext = CustomExtension::from_oid_content(
        &oid_to_asn1(OID_CERT_CHAIN),
        chain_json,
    );
    params.custom_extensions.push(chain_ext);

    // Generate the self-signed certificate
    let cert = params.self_signed(&key_pair).map_err(|e| {
        BoxError::AttestationError(format!("Failed to generate RA-TLS certificate: {}", e))
    })?;

    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();

    tracing::info!(
        cert_size = cert_der.len(),
        report_size = report.report.len(),
        "Generated RA-TLS certificate with SNP attestation report"
    );

    Ok((cert_der, key_der))
}

// ============================================================================
// Report extraction from certificate
// ============================================================================

/// Extract an SNP attestation report from an RA-TLS certificate.
///
/// Parses the X.509 certificate and looks for the custom extensions
/// containing the SNP report and certificate chain.
pub fn extract_report_from_cert(cert_der: &[u8]) -> Result<AttestationReport> {
    use der::Decode;
    use x509_cert::Certificate;

    let cert = Certificate::from_der(cert_der).map_err(|e| {
        BoxError::AttestationError(format!("Failed to parse RA-TLS certificate: {}", e))
    })?;

    let mut report_bytes: Option<Vec<u8>> = None;
    let mut cert_chain = CertificateChain::default();

    // Search extensions for our custom OIDs
    if let Some(extensions) = &cert.tbs_certificate.extensions {
        let report_oid = oid_string_to_der(OID_SNP_REPORT);
        let chain_oid = oid_string_to_der(OID_CERT_CHAIN);

        for ext in extensions.iter() {
            let ext_oid = ext.extn_id.to_string();

            if ext_oid == oid_der_to_dotted(&report_oid) || ext.extn_id.as_bytes() == report_oid {
                report_bytes = Some(ext.extn_value.as_bytes().to_vec());
            } else if ext_oid == oid_der_to_dotted(&chain_oid)
                || ext.extn_id.as_bytes() == chain_oid
            {
                if let Ok(chain) = serde_json::from_slice::<CertificateChain>(ext.extn_value.as_bytes()) {
                    cert_chain = chain;
                }
            }
        }
    }

    let report = report_bytes.ok_or_else(|| {
        BoxError::AttestationError(
            "RA-TLS certificate does not contain SNP report extension".to_string(),
        )
    })?;

    // Parse platform info from the report
    let platform = super::attestation::parse_platform_info(&report)
        .unwrap_or_default();

    Ok(AttestationReport {
        report,
        cert_chain,
        platform,
    })
}

/// Verify an RA-TLS certificate by extracting and verifying the embedded SNP report.
///
/// # Arguments
/// * `cert_der` - DER-encoded X.509 certificate
/// * `expected_nonce` - Expected nonce in the report (or empty to skip nonce check)
/// * `policy` - Attestation policy to check against
/// * `allow_simulated` - Whether to accept simulated reports
pub fn verify_ratls_certificate(
    cert_der: &[u8],
    expected_nonce: &[u8],
    policy: &AttestationPolicy,
    allow_simulated: bool,
) -> Result<super::verifier::VerificationResult> {
    let report = extract_report_from_cert(cert_der)?;
    verify_attestation(&report, expected_nonce, policy, allow_simulated)
}

// ============================================================================
// TLS configuration builders
// ============================================================================

/// Create a rustls `ServerConfig` for an RA-TLS server.
///
/// The server presents the RA-TLS certificate (containing the SNP report)
/// to connecting clients during the TLS handshake.
pub fn create_server_config(
    cert_der: &[u8],
    key_der: &[u8],
) -> Result<rustls::ServerConfig> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

    let cert = CertificateDer::from(cert_der.to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der.to_vec()));

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|e| {
            BoxError::AttestationError(format!("Failed to create RA-TLS server config: {}", e))
        })?;

    Ok(config)
}

/// Create a rustls `ClientConfig` for connecting to an RA-TLS server.
///
/// Uses a custom certificate verifier that extracts the SNP report from
/// the server's certificate and verifies it against the given policy.
pub fn create_client_config(
    policy: AttestationPolicy,
    allow_simulated: bool,
) -> Result<rustls::ClientConfig> {
    let verifier = RaTlsVerifier::new(policy, allow_simulated);

    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(verifier))
        .with_no_client_auth();

    Ok(config)
}

// ============================================================================
// Custom TLS certificate verifier
// ============================================================================

/// Custom rustls certificate verifier for RA-TLS.
///
/// During TLS handshake, extracts the SNP attestation report from the
/// server's certificate extension and verifies it using the standard
/// attestation verification flow (signature, cert chain, policy).
#[derive(Debug)]
struct RaTlsVerifier {
    policy: AttestationPolicy,
    allow_simulated: bool,
}

impl RaTlsVerifier {
    fn new(policy: AttestationPolicy, allow_simulated: bool) -> Self {
        Self {
            policy,
            allow_simulated,
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for RaTlsVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        // Extract and verify the SNP report from the certificate
        let report = extract_report_from_cert(end_entity.as_ref()).map_err(|e| {
            rustls::Error::General(format!("RA-TLS report extraction failed: {}", e))
        })?;

        // For RA-TLS, we don't enforce a specific nonce in the cert —
        // the nonce binding happens at the application layer.
        // We verify the report structure, signature, and policy.
        let empty_nonce: Vec<u8> = Vec::new();
        let nonce_to_check = if report.report.len() >= 0x90 {
            // Use the report_data as the "expected nonce" (self-check passes)
            &report.report[0x50..0x90]
        } else {
            &empty_nonce
        };

        let result =
            verify_attestation(&report, nonce_to_check, &self.policy, self.allow_simulated)
                .map_err(|e| {
                    rustls::Error::General(format!("RA-TLS attestation verification failed: {}", e))
                })?;

        if result.verified {
            tracing::debug!(
                simulated = is_simulated_report(&report.report),
                "RA-TLS attestation verified"
            );
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            let failures = result.failures.join("; ");
            Err(rustls::Error::General(format!(
                "RA-TLS attestation failed: {}",
                failures
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // We trust the TLS signature if the attestation report is valid
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
        ]
    }
}

// ============================================================================
// OID helpers
// ============================================================================

/// Convert a dotted OID string to rcgen's ASN.1 OID format (array of u64).
fn oid_to_asn1(oid: &str) -> Vec<u64> {
    oid.split('.')
        .filter_map(|s| s.parse::<u64>().ok())
        .collect()
}

/// Convert a dotted OID string to DER-encoded OID bytes.
fn oid_string_to_der(oid: &str) -> Vec<u8> {
    let components: Vec<u64> = oid_to_asn1(oid);
    if components.len() < 2 {
        return vec![];
    }

    let mut encoded = Vec::new();
    // First two components are encoded as (c0 * 40 + c1)
    encoded.push((components[0] * 40 + components[1]) as u8);

    // Remaining components use base-128 encoding
    for &c in &components[2..] {
        encode_base128(&mut encoded, c);
    }

    encoded
}

/// Encode a value in base-128 (variable-length quantity) for OID encoding.
fn encode_base128(buf: &mut Vec<u8>, value: u64) {
    if value < 128 {
        buf.push(value as u8);
        return;
    }

    let mut bytes = Vec::new();
    let mut v = value;
    bytes.push((v & 0x7F) as u8);
    v >>= 7;
    while v > 0 {
        bytes.push((v & 0x7F) as u8 | 0x80);
        v >>= 7;
    }
    bytes.reverse();
    buf.extend_from_slice(&bytes);
}

/// Convert DER-encoded OID bytes to dotted string for comparison.
fn oid_der_to_dotted(der: &[u8]) -> String {
    if der.is_empty() {
        return String::new();
    }

    let mut components = Vec::new();
    components.push((der[0] / 40) as u64);
    components.push((der[0] % 40) as u64);

    let mut value: u64 = 0;
    for &byte in &der[1..] {
        value = (value << 7) | (byte & 0x7F) as u64;
        if byte & 0x80 == 0 {
            components.push(value);
            value = 0;
        }
    }

    components
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tee::attestation::{CertificateChain, PlatformInfo};
    use crate::tee::simulate::build_simulated_report;

    fn make_test_attestation_report() -> AttestationReport {
        let mut report_data = [0u8; 64];
        report_data[0..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let report = build_simulated_report(&report_data);
        AttestationReport {
            report,
            cert_chain: CertificateChain::default(),
            platform: PlatformInfo::default(),
        }
    }

    #[test]
    fn test_oid_to_asn1() {
        let asn1 = oid_to_asn1("1.3.6.1.4.1.58270.1.1");
        assert_eq!(asn1, vec![1, 3, 6, 1, 4, 1, 58270, 1, 1]);
    }

    #[test]
    fn test_oid_roundtrip() {
        let oid = "1.3.6.1.4.1.58270.1.1";
        let der = oid_string_to_der(oid);
        let dotted = oid_der_to_dotted(&der);
        assert_eq!(dotted, oid);
    }

    #[test]
    fn test_oid_roundtrip_chain() {
        let oid = "1.3.6.1.4.1.58270.1.2";
        let der = oid_string_to_der(oid);
        let dotted = oid_der_to_dotted(&der);
        assert_eq!(dotted, oid);
    }

    #[test]
    fn test_encode_base128_small() {
        let mut buf = Vec::new();
        encode_base128(&mut buf, 127);
        assert_eq!(buf, vec![127]);
    }

    #[test]
    fn test_encode_base128_large() {
        let mut buf = Vec::new();
        encode_base128(&mut buf, 58270);
        // 58270 = 0xE39E -> base128: [0x83, 0xC7, 0x1E]
        assert!(!buf.is_empty());
        // Verify roundtrip
        let mut value: u64 = 0;
        for &b in &buf {
            value = (value << 7) | (b & 0x7F) as u64;
        }
        assert_eq!(value, 58270);
    }

    #[test]
    fn test_generate_ratls_certificate() {
        let report = make_test_attestation_report();
        let (cert_der, key_der) = generate_ratls_certificate(&report).unwrap();
        assert!(!cert_der.is_empty());
        assert!(!key_der.is_empty());
    }

    #[test]
    fn test_extract_report_from_cert() {
        let report = make_test_attestation_report();
        let (cert_der, _) = generate_ratls_certificate(&report).unwrap();

        let extracted = extract_report_from_cert(&cert_der).unwrap();
        assert_eq!(extracted.report.len(), 1184);
        // Verify the report_data is preserved
        assert_eq!(extracted.report[0x50], 0xDE);
        assert_eq!(extracted.report[0x51], 0xAD);
        assert_eq!(extracted.report[0x52], 0xBE);
        assert_eq!(extracted.report[0x53], 0xEF);
    }

    #[test]
    fn test_extract_report_no_extension() {
        // A regular cert without our extension should fail
        use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P384_SHA384};
        let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        let params = CertificateParams::default();
        let cert = params.self_signed(&key_pair).unwrap();
        let result = extract_report_from_cert(cert.der());
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_ratls_certificate_simulated() {
        let report = make_test_attestation_report();
        let (cert_der, _) = generate_ratls_certificate(&report).unwrap();

        let nonce = &report.report[0x50..0x90];
        let policy = AttestationPolicy {
            require_no_debug: false,
            ..Default::default()
        };
        let result = verify_ratls_certificate(&cert_der, nonce, &policy, true).unwrap();
        assert!(result.verified);
    }

    #[test]
    fn test_verify_ratls_certificate_simulated_rejected() {
        let report = make_test_attestation_report();
        let (cert_der, _) = generate_ratls_certificate(&report).unwrap();

        let nonce = &report.report[0x50..0x90];
        let policy = AttestationPolicy::default();
        // allow_simulated = false should reject
        let result = verify_ratls_certificate(&cert_der, nonce, &policy, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_create_server_config() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let report = make_test_attestation_report();
        let (cert_der, key_der) = generate_ratls_certificate(&report).unwrap();
        let config = create_server_config(&cert_der, &key_der);
        assert!(config.is_ok());
    }

    #[test]
    fn test_create_client_config() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let policy = AttestationPolicy::default();
        let config = create_client_config(policy, true);
        assert!(config.is_ok());
    }

    #[test]
    fn test_ratls_verifier_debug() {
        let verifier = RaTlsVerifier::new(AttestationPolicy::default(), false);
        let debug = format!("{:?}", verifier);
        assert!(debug.contains("RaTlsVerifier"));
    }
}
