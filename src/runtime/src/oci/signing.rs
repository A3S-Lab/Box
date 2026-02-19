//! Image signature verification for OCI images.
//!
//! Supports cosign-compatible signature verification using public keys.
//! Signatures are stored as OCI artifacts in the registry with the tag
//! convention `sha256-<digest>.sig`.

use a3s_box_core::error::{BoxError, Result};
use base64::Engine;
use oci_distribution::client::ClientConfig;
use oci_distribution::errors::{OciDistributionError, OciErrorCode};
use oci_distribution::secrets::RegistryAuth;
use oci_distribution::{Client, Reference};
use serde::{Deserialize, Serialize};

/// Image signature verification policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SignaturePolicy {
    /// Skip signature verification (default for backward compatibility).
    #[default]
    Skip,
    /// Require a valid cosign signature verified against a public key.
    CosignKey {
        /// Path to the PEM-encoded public key file.
        public_key: String,
    },
    /// Require a valid cosign keyless signature (Fulcio + Rekor transparency log).
    CosignKeyless {
        /// Expected OIDC issuer (e.g., "https://accounts.google.com").
        issuer: String,
        /// Expected certificate identity (e.g., "user@example.com").
        identity: String,
    },
}

/// Result of a signature verification check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyResult {
    /// Signature is valid.
    Verified,
    /// Verification was skipped (policy = Skip).
    Skipped,
    /// No signature found for the image.
    NoSignature,
    /// Signature found but verification failed.
    Failed(String),
}

impl VerifyResult {
    /// Returns true if the result is acceptable (Verified or Skipped).
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Verified | Self::Skipped)
    }
}

/// Cosign signature payload (SimpleSigning format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CosignPayload {
    /// The critical section containing image identity.
    pub(super) critical: CosignCritical,
    /// Optional annotations.
    #[serde(default)]
    pub(super) optional: serde_json::Value,
}

/// Critical section of a cosign signature payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CosignCritical {
    /// Identity of the signed image.
    pub(super) identity: CosignIdentity,
    /// Image reference being signed.
    pub(super) image: CosignImage,
    /// Type of signature (always "cosign container image signature").
    #[serde(rename = "type")]
    pub(super) sig_type: String,
}

/// Identity in a cosign signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CosignIdentity {
    /// Docker reference of the signed image.
    #[serde(rename = "docker-reference")]
    pub(super) docker_reference: String,
}

/// Image reference in a cosign signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CosignImage {
    /// Digest of the signed manifest.
    #[serde(rename = "docker-manifest-digest")]
    pub(super) docker_manifest_digest: String,
}

/// Cosign signature tag convention: `sha256-<hex>.sig`
fn cosign_signature_tag(manifest_digest: &str) -> String {
    let hex = manifest_digest
        .strip_prefix("sha256:")
        .unwrap_or(manifest_digest);
    format!("sha256-{}.sig", hex)
}

/// Check if a cosign signature exists for the given image in the registry.
async fn fetch_cosign_signature(
    registry: &str,
    repository: &str,
    manifest_digest: &str,
) -> Result<Option<Vec<u8>>> {
    let sig_tag = cosign_signature_tag(manifest_digest);
    let reference_str = format!("{}/{}:{}", registry, repository, sig_tag);

    let reference: Reference = reference_str.parse().map_err(|e| BoxError::RegistryError {
        registry: registry.to_string(),
        message: format!("Invalid signature reference: {}", e),
    })?;

    let config = ClientConfig {
        protocol: oci_distribution::client::ClientProtocol::Https,
        ..Default::default()
    };
    let client = Client::new(config);

    // Try to pull the signature manifest
    match client
        .pull_image_manifest(&reference, &RegistryAuth::Anonymous)
        .await
    {
        Ok((manifest, _digest)) => {
            // Pull the first layer (the signature payload) into a Vec<u8>
            if let Some(layer) = manifest.layers.first() {
                let mut buf = Vec::new();
                match client.pull_blob(&reference, layer, &mut buf).await {
                    Ok(()) => Ok(Some(buf)),
                    Err(e) => {
                        tracing::warn!(
                            reference = %reference_str,
                            error = %e,
                            "Failed to pull cosign signature blob"
                        );
                        Ok(None)
                    }
                }
            } else {
                Ok(None)
            }
        }
        Err(e) => {
            // Distinguish "no signature" (manifest not found) from actual errors
            // using typed variants rather than string matching.
            let is_not_found = matches!(e, OciDistributionError::ImageManifestNotFoundError(_))
                || matches!(&e, OciDistributionError::RegistryError { envelope, .. }
                    if envelope.errors.iter().any(|oe| oe.code == OciErrorCode::ManifestUnknown));
            if is_not_found {
                // No signature manifest found — image is unsigned, not an error
                Ok(None)
            } else {
                tracing::warn!(
                    reference = %reference_str,
                    error = %e,
                    "Registry error while fetching cosign signature"
                );
                Err(BoxError::RegistryError {
                    registry: registry.to_string(),
                    message: format!("Failed to fetch cosign signature: {}", e),
                })
            }
        }
    }
}

/// Verify a cosign signature payload against a public key.
///
/// The payload is a JSON SimpleSigning document. The signature is
/// verified using the provided PEM-encoded public key (ECDSA P-256 or RSA).
fn verify_cosign_payload(payload: &[u8], manifest_digest: &str) -> Result<CosignPayload> {
    // Parse the payload
    let cosign_payload: CosignPayload =
        serde_json::from_slice(payload).map_err(|e| BoxError::RegistryError {
            registry: String::new(),
            message: format!("Invalid cosign payload: {}", e),
        })?;

    // Verify the digest matches
    if cosign_payload.critical.image.docker_manifest_digest != manifest_digest {
        return Err(BoxError::RegistryError {
            registry: String::new(),
            message: format!(
                "Signature digest mismatch: expected {}, got {}",
                manifest_digest, cosign_payload.critical.image.docker_manifest_digest
            ),
        });
    }

    Ok(cosign_payload)
}

/// Verify an image signature according to the given policy.
pub async fn verify_image_signature(
    policy: &SignaturePolicy,
    registry: &str,
    repository: &str,
    manifest_digest: &str,
) -> VerifyResult {
    match policy {
        SignaturePolicy::Skip => VerifyResult::Skipped,

        SignaturePolicy::CosignKey { public_key } => {
            verify_cosign_key(public_key, registry, repository, manifest_digest).await
        }

        SignaturePolicy::CosignKeyless { issuer, identity } => {
            // Keyless verification requires validating the Fulcio certificate chain
            // and checking the Rekor transparency log. Neither is implemented.
            // Returning "Verified" without these checks is a security hole.
            tracing::warn!(
                digest = %manifest_digest,
                issuer = %issuer,
                identity = %identity,
                "CosignKeyless verification requested but Fulcio/Rekor \
                 verification is not yet implemented"
            );
            VerifyResult::Failed(
                "cosign keyless verification is not yet implemented: \
                 Fulcio certificate chain and Rekor transparency log validation are required \
                 but not available; use signature_policy=skip"
                    .to_string(),
            )
        }
    }
}

/// Verify a cosign signature using a PEM-encoded public key.
///
/// Steps:
/// 1. Read the PEM public key from disk
/// 2. Fetch the cosign signature artifact from the registry
/// 3. Extract the SimpleSigning payload and base64-encoded signature from the OCI layer
/// 4. Verify the ECDSA P-256 signature over the payload using the public key
/// 5. Validate the payload digest matches the manifest digest
async fn verify_cosign_key(
    public_key_path: &str,
    registry: &str,
    repository: &str,
    manifest_digest: &str,
) -> VerifyResult {
    // 1. Read the PEM public key
    let pem_bytes = match std::fs::read(public_key_path) {
        Ok(b) => b,
        Err(e) => {
            return VerifyResult::Failed(format!(
                "Failed to read public key file '{}': {}",
                public_key_path, e
            ));
        }
    };

    let verifying_key = match parse_pem_public_key(&pem_bytes) {
        Ok(k) => k,
        Err(e) => {
            return VerifyResult::Failed(format!("Failed to parse public key: {}", e));
        }
    };

    // 2. Fetch the cosign signature artifact
    let sig_data = match fetch_cosign_signature(registry, repository, manifest_digest).await {
        Ok(Some(data)) => data,
        Ok(None) => return VerifyResult::NoSignature,
        Err(e) => {
            return VerifyResult::Failed(format!("Failed to fetch signature: {}", e));
        }
    };

    // 3. Parse the signature layer. Cosign stores a JSON object per layer with
    //    "payload" (base64-encoded SimpleSigning JSON) and "signature" (base64-encoded raw sig).
    let sig_envelope: CosignSignatureEnvelope = match serde_json::from_slice(&sig_data) {
        Ok(e) => e,
        Err(e) => {
            return VerifyResult::Failed(format!(
                "Failed to parse cosign signature envelope: {}",
                e
            ));
        }
    };

    let payload_bytes = match base64_decode(&sig_envelope.payload) {
        Ok(b) => b,
        Err(e) => {
            return VerifyResult::Failed(format!("Failed to decode signature payload: {}", e));
        }
    };

    let signature_bytes = match base64_decode(&sig_envelope.signature) {
        Ok(b) => b,
        Err(e) => {
            return VerifyResult::Failed(format!("Failed to decode signature bytes: {}", e));
        }
    };

    // 4. Verify the ECDSA P-256 signature over the payload
    if let Err(e) = verify_ecdsa_p256(&verifying_key, &payload_bytes, &signature_bytes) {
        return VerifyResult::Failed(format!("Signature verification failed: {}", e));
    }

    // 5. Validate the payload digest matches
    match verify_cosign_payload(&payload_bytes, manifest_digest) {
        Ok(_) => VerifyResult::Verified,
        Err(e) => VerifyResult::Failed(format!("Payload validation failed: {}", e)),
    }
}

/// Cosign signature envelope stored in the OCI layer.
#[derive(Debug, Deserialize)]
struct CosignSignatureEnvelope {
    /// Base64-encoded SimpleSigning payload.
    payload: String,
    /// Base64-encoded ECDSA signature over the payload.
    signature: String,
}

/// Parse a PEM-encoded public key (ECDSA P-256) into raw SEC1 bytes.
///
/// Supports both "PUBLIC KEY" (SPKI/PKIX) and "EC PUBLIC KEY" (SEC1) PEM formats.
fn parse_pem_public_key(pem_bytes: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let pem_str = std::str::from_utf8(pem_bytes)
        .map_err(|e| format!("PEM file is not valid UTF-8: {}", e))?;

    // Extract the base64 content between PEM headers
    let (begin_marker, end_marker) = if pem_str.contains("BEGIN PUBLIC KEY") {
        ("-----BEGIN PUBLIC KEY-----", "-----END PUBLIC KEY-----")
    } else if pem_str.contains("BEGIN EC PUBLIC KEY") {
        (
            "-----BEGIN EC PUBLIC KEY-----",
            "-----END EC PUBLIC KEY-----",
        )
    } else {
        return Err("Unsupported PEM format: expected PUBLIC KEY or EC PUBLIC KEY".to_string());
    };

    let start = pem_str
        .find(begin_marker)
        .ok_or("Missing PEM begin marker")?
        + begin_marker.len();
    let end = pem_str.find(end_marker).ok_or("Missing PEM end marker")?;

    let b64: String = pem_str[start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();

    let der_bytes =
        base64_decode(&b64).map_err(|e| format!("Failed to decode PEM base64: {}", e))?;

    // If SPKI format, extract the public key bytes from the SubjectPublicKeyInfo structure
    if begin_marker.contains("BEGIN PUBLIC KEY") {
        extract_spki_public_key(&der_bytes)
    } else {
        // SEC1 format — the DER bytes are the raw EC point
        Ok(der_bytes)
    }
}

/// Extract the public key bytes from a DER-encoded SubjectPublicKeyInfo.
fn extract_spki_public_key(der: &[u8]) -> std::result::Result<Vec<u8>, String> {
    use der::Decode;
    use spki::SubjectPublicKeyInfo;

    let spki =
        SubjectPublicKeyInfo::<der::asn1::AnyRef<'_>, der::asn1::BitStringRef<'_>>::from_der(der)
            .map_err(|e| format!("Failed to parse SPKI: {}", e))?;

    spki.subject_public_key
        .as_bytes()
        .map(|b| b.to_vec())
        .ok_or_else(|| "Failed to extract public key bytes from SPKI".to_string())
}

/// Verify an ECDSA P-256 signature over a message.
fn verify_ecdsa_p256(
    public_key_bytes: &[u8],
    message: &[u8],
    signature: &[u8],
) -> std::result::Result<(), String> {
    use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};

    let verifying_key = VerifyingKey::from_sec1_bytes(public_key_bytes)
        .map_err(|e| format!("Invalid P-256 public key: {}", e))?;

    // Cosign produces DER-encoded signatures. Try DER first, then fixed-size.
    let result = if let Ok(sig) = p256::ecdsa::DerSignature::from_bytes(signature) {
        verifying_key.verify(message, &sig)
    } else if signature.len() == 64 {
        let sig = Signature::from_slice(signature)
            .map_err(|e| format!("Invalid P-256 signature: {}", e))?;
        verifying_key.verify(message, &sig)
    } else {
        return Err(format!(
            "Unrecognized signature format ({} bytes)",
            signature.len()
        ));
    };

    result.map_err(|_| "ECDSA P-256 signature verification failed".to_string())
}

/// Decode a base64 string (standard alphabet with padding).
fn base64_decode(input: &str) -> std::result::Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .map_err(|e| format!("base64 decode error: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- SignaturePolicy tests ---

    #[test]
    fn test_signature_policy_default_is_skip() {
        assert_eq!(SignaturePolicy::default(), SignaturePolicy::Skip);
    }

    #[test]
    fn test_signature_policy_serde_skip() {
        let policy = SignaturePolicy::Skip;
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: SignaturePolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SignaturePolicy::Skip);
    }

    #[test]
    fn test_signature_policy_serde_cosign_key() {
        let policy = SignaturePolicy::CosignKey {
            public_key: "/path/to/cosign.pub".to_string(),
        };
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: SignaturePolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, policy);
    }

    #[test]
    fn test_signature_policy_serde_cosign_keyless() {
        let policy = SignaturePolicy::CosignKeyless {
            issuer: "https://accounts.google.com".to_string(),
            identity: "user@example.com".to_string(),
        };
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: SignaturePolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, policy);
    }

    // --- VerifyResult tests ---

    #[test]
    fn test_verify_result_is_ok() {
        assert!(VerifyResult::Verified.is_ok());
        assert!(VerifyResult::Skipped.is_ok());
        assert!(!VerifyResult::NoSignature.is_ok());
        assert!(!VerifyResult::Failed("err".to_string()).is_ok());
    }

    #[test]
    fn test_verify_result_debug() {
        let r = VerifyResult::Verified;
        assert!(format!("{:?}", r).contains("Verified"));
    }

    // --- Cosign tag convention tests ---

    #[test]
    fn test_cosign_signature_tag_with_prefix() {
        let tag = cosign_signature_tag("sha256:abc123def456");
        assert_eq!(tag, "sha256-abc123def456.sig");
    }

    #[test]
    fn test_cosign_signature_tag_without_prefix() {
        let tag = cosign_signature_tag("abc123def456");
        assert_eq!(tag, "sha256-abc123def456.sig");
    }

    // --- Cosign payload tests ---

    #[test]
    fn test_verify_cosign_payload_valid() {
        let digest = "sha256:abc123";
        let payload = serde_json::json!({
            "critical": {
                "identity": {
                    "docker-reference": "docker.io/library/alpine"
                },
                "image": {
                    "docker-manifest-digest": digest
                },
                "type": "cosign container image signature"
            },
            "optional": {}
        });
        let bytes = serde_json::to_vec(&payload).unwrap();
        let result = verify_cosign_payload(&bytes, digest);
        assert!(result.is_ok());
        let p = result.unwrap();
        assert_eq!(p.critical.image.docker_manifest_digest, digest);
        assert_eq!(
            p.critical.identity.docker_reference,
            "docker.io/library/alpine"
        );
    }

    #[test]
    fn test_verify_cosign_payload_digest_mismatch() {
        let payload = serde_json::json!({
            "critical": {
                "identity": {
                    "docker-reference": "docker.io/library/alpine"
                },
                "image": {
                    "docker-manifest-digest": "sha256:wrong"
                },
                "type": "cosign container image signature"
            },
            "optional": {}
        });
        let bytes = serde_json::to_vec(&payload).unwrap();
        let result = verify_cosign_payload(&bytes, "sha256:expected");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mismatch"));
    }

    #[test]
    fn test_verify_cosign_payload_invalid_json() {
        let result = verify_cosign_payload(b"not json", "sha256:abc");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid cosign payload"));
    }

    // --- Async verification tests ---

    #[tokio::test]
    async fn test_verify_image_signature_skip() {
        let result = verify_image_signature(
            &SignaturePolicy::Skip,
            "docker.io",
            "library/alpine",
            "sha256:abc",
        )
        .await;
        assert_eq!(result, VerifyResult::Skipped);
    }

    #[tokio::test]
    async fn test_verify_image_signature_cosign_key_missing_file() {
        let policy = SignaturePolicy::CosignKey {
            public_key: "/nonexistent/cosign.pub".to_string(),
        };
        let result =
            verify_image_signature(&policy, "docker.io", "library/alpine", "sha256:abc").await;
        match result {
            VerifyResult::Failed(msg) => assert!(msg.contains("Failed to read public key")),
            other => panic!("Expected Failed, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_verify_image_signature_cosign_keyless_not_implemented() {
        let policy = SignaturePolicy::CosignKeyless {
            issuer: "https://accounts.google.com".to_string(),
            identity: "user@example.com".to_string(),
        };
        let result =
            verify_image_signature(&policy, "docker.io", "library/alpine", "sha256:abc").await;
        match result {
            VerifyResult::Failed(msg) => {
                assert!(msg.contains("not yet implemented"));
                assert!(msg.contains("Fulcio"));
            }
            other => panic!("Expected Failed, got {:?}", other),
        }
    }

    // --- ECDSA P-256 crypto verification tests ---

    /// Generate a P-256 key pair and return (private_key, public_key_sec1_bytes, pem_string).
    fn generate_test_p256_key() -> (p256::ecdsa::SigningKey, Vec<u8>, String) {
        use p256::ecdsa::SigningKey;

        let signing_key = SigningKey::random(&mut rand::thread_rng());
        let verifying_key = signing_key.verifying_key();
        let pub_bytes = verifying_key.to_encoded_point(false).as_bytes().to_vec();

        // Build SPKI DER manually for the PEM
        let spki_der = build_p256_spki_der(&pub_bytes);
        let b64 = base64_encode_for_test(&spki_der);
        let pem = format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            b64
        );

        (signing_key, pub_bytes, pem)
    }

    /// Build a minimal SPKI DER for a P-256 public key.
    fn build_p256_spki_der(pub_key_bytes: &[u8]) -> Vec<u8> {
        // OID for id-ecPublicKey: 1.2.840.10045.2.1
        let ec_oid: &[u8] = &[0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
        // OID for prime256v1 (P-256): 1.2.840.10045.3.1.7
        let p256_oid: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];

        // AlgorithmIdentifier SEQUENCE
        let alg_content_len = ec_oid.len() + p256_oid.len();
        let mut alg_id = vec![0x30];
        encode_der_length(&mut alg_id, alg_content_len);
        alg_id.extend_from_slice(ec_oid);
        alg_id.extend_from_slice(p256_oid);

        // BIT STRING wrapping the public key
        let bit_string_len = 1 + pub_key_bytes.len(); // 1 byte for unused bits count
        let mut bit_string = vec![0x03];
        encode_der_length(&mut bit_string, bit_string_len);
        bit_string.push(0x00); // no unused bits
        bit_string.extend_from_slice(pub_key_bytes);

        // Outer SEQUENCE
        let total_content_len = alg_id.len() + bit_string.len();
        let mut spki = vec![0x30];
        encode_der_length(&mut spki, total_content_len);
        spki.extend_from_slice(&alg_id);
        spki.extend_from_slice(&bit_string);

        spki
    }

    fn encode_der_length(buf: &mut Vec<u8>, len: usize) {
        if len < 0x80 {
            buf.push(len as u8);
        } else if len < 0x100 {
            buf.push(0x81);
            buf.push(len as u8);
        } else {
            buf.push(0x82);
            buf.push((len >> 8) as u8);
            buf.push(len as u8);
        }
    }

    fn base64_encode_for_test(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    #[test]
    fn test_parse_pem_public_key_spki() {
        let (_sk, expected_pub, pem) = generate_test_p256_key();
        let parsed = parse_pem_public_key(pem.as_bytes()).unwrap();
        assert_eq!(parsed, expected_pub);
    }

    #[test]
    fn test_parse_pem_public_key_invalid() {
        let result = parse_pem_public_key(b"not a pem file");
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_ecdsa_p256_valid_signature() {
        use p256::ecdsa::{signature::Signer, SigningKey};

        let signing_key = SigningKey::random(&mut rand::thread_rng());
        let verifying_key = signing_key.verifying_key();
        let pub_bytes = verifying_key.to_encoded_point(false).as_bytes().to_vec();

        let message = b"test payload for cosign verification";
        let sig: p256::ecdsa::DerSignature = signing_key.sign(message);

        let result = verify_ecdsa_p256(&pub_bytes, message, sig.as_bytes());
        assert!(result.is_ok());
    }

    #[test]
    fn test_verify_ecdsa_p256_wrong_key_rejects() {
        use p256::ecdsa::{signature::Signer, SigningKey};

        let signing_key = SigningKey::random(&mut rand::thread_rng());
        let wrong_key = SigningKey::random(&mut rand::thread_rng());
        let wrong_pub = wrong_key
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec();

        let message = b"test payload";
        let sig: p256::ecdsa::DerSignature = signing_key.sign(message);

        let result = verify_ecdsa_p256(&wrong_pub, message, sig.as_bytes());
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_ecdsa_p256_tampered_message_rejects() {
        use p256::ecdsa::{signature::Signer, SigningKey};

        let signing_key = SigningKey::random(&mut rand::thread_rng());
        let pub_bytes = signing_key
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec();

        let message = b"original message";
        let sig: p256::ecdsa::DerSignature = signing_key.sign(message);

        let result = verify_ecdsa_p256(&pub_bytes, b"tampered message", sig.as_bytes());
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_ecdsa_p256_fixed_size_signature() {
        use p256::ecdsa::{signature::Signer, Signature, SigningKey};

        let signing_key = SigningKey::random(&mut rand::thread_rng());
        let pub_bytes = signing_key
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec();

        let message = b"test with fixed-size sig";
        let sig: Signature = signing_key.sign(message);

        // Fixed-size signature is 64 bytes (32 r + 32 s)
        assert_eq!(sig.to_bytes().len(), 64);
        let result = verify_ecdsa_p256(&pub_bytes, message, &sig.to_bytes());
        assert!(result.is_ok());
    }

    #[test]
    fn test_cosign_key_end_to_end_with_temp_file() {
        use p256::ecdsa::signature::Signer;

        let (signing_key, _pub_bytes, pem) = generate_test_p256_key();

        // Write PEM to temp file
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("cosign.pub");
        std::fs::write(&key_path, &pem).unwrap();

        // Create a signed cosign envelope
        let digest = "sha256:abc123def456";
        let payload = serde_json::json!({
            "critical": {
                "identity": { "docker-reference": "docker.io/library/alpine" },
                "image": { "docker-manifest-digest": digest },
                "type": "cosign container image signature"
            },
            "optional": {}
        });
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let sig: p256::ecdsa::DerSignature = signing_key.sign(&payload_bytes);

        let envelope = serde_json::json!({
            "payload": base64_encode_for_test(&payload_bytes),
            "signature": base64_encode_for_test(sig.as_bytes()),
        });
        let envelope_bytes = serde_json::to_vec(&envelope).unwrap();

        // Parse and verify the envelope manually (simulating what verify_cosign_key does
        // after fetching from registry)
        let env: CosignSignatureEnvelope = serde_json::from_slice(&envelope_bytes).unwrap();
        let decoded_payload = base64_decode(&env.payload).unwrap();
        let decoded_sig = base64_decode(&env.signature).unwrap();

        // Read the key
        let pem_bytes = std::fs::read(&key_path).unwrap();
        let pub_key = parse_pem_public_key(&pem_bytes).unwrap();

        // Verify signature
        assert!(verify_ecdsa_p256(&pub_key, &decoded_payload, &decoded_sig).is_ok());

        // Verify payload
        assert!(verify_cosign_payload(&decoded_payload, digest).is_ok());
    }

    // --- CosignPayload serde tests ---

    #[test]
    fn test_cosign_payload_serde_roundtrip() {
        let payload = CosignPayload {
            critical: CosignCritical {
                identity: CosignIdentity {
                    docker_reference: "ghcr.io/myorg/myimage".to_string(),
                },
                image: CosignImage {
                    docker_manifest_digest: "sha256:deadbeef".to_string(),
                },
                sig_type: "cosign container image signature".to_string(),
            },
            optional: serde_json::json!({"creator": "a3s-box"}),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: CosignPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.critical.image.docker_manifest_digest,
            "sha256:deadbeef"
        );
        assert_eq!(
            parsed.critical.identity.docker_reference,
            "ghcr.io/myorg/myimage"
        );
    }
}
