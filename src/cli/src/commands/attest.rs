//! `a3s-box attest` command — Request and verify a TEE attestation report.
//!
//! Connects to a running box's agent socket, requests a hardware-signed
//! SNP attestation report, optionally verifies it against a policy, and
//! outputs the result as JSON.

use a3s_box_core::error::BoxError;
use clap::Args;
use std::path::PathBuf;

#[cfg(not(windows))]
use crate::resolve;
#[cfg(not(windows))]
use crate::state::StateFile;

#[cfg(not(windows))]
use a3s_box_runtime::{verify_attestation, AttestationPolicy, RaTlsAttestationClient};

#[derive(Args)]
pub struct AttestArgs {
    /// Box name or ID
    pub r#box: String,

    /// Path to attestation policy JSON file.
    /// If not provided, a default policy (require_no_debug=true) is used.
    #[arg(long, short)]
    pub policy: Option<PathBuf>,

    /// Custom nonce (hex-encoded). If not provided, a random nonce is generated.
    #[arg(long)]
    pub nonce: Option<String>,

    /// Output raw report without verification (skip signature/policy checks).
    #[arg(long)]
    pub raw: bool,

    /// Accept simulated (non-hardware) TEE reports for development/testing.
    #[arg(long)]
    pub allow_simulated: bool,

    /// Use RA-TLS for attestation verification (recommended).
    /// Verifies the TEE during the TLS handshake instead of fetching a raw report.
    #[arg(long)]
    pub ratls: bool,

    /// Only output the verification result (true/false), no full report.
    #[arg(long, short)]
    pub quiet: bool,
}

/// JSON output for the attest command.
#[cfg(not(windows))]
#[derive(serde::Serialize)]
struct AttestOutput {
    /// Box ID
    box_id: String,
    /// Box name
    box_name: String,
    /// Whether verification passed (None if --raw)
    #[serde(skip_serializing_if = "Option::is_none")]
    verified: Option<bool>,
    /// Platform info from the report
    #[serde(skip_serializing_if = "Option::is_none")]
    platform: Option<a3s_box_runtime::PlatformInfo>,
    /// Nonce used (hex-encoded)
    nonce: String,
    /// Raw report (hex-encoded)
    #[serde(skip_serializing_if = "Option::is_none")]
    report_hex: Option<String>,
    /// Verification failures (empty if passed)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failures: Vec<String>,
}

#[cfg(windows)]
pub async fn execute(_args: AttestArgs) -> Result<(), BoxError> {
    Err(BoxError::ConfigError(
        crate::platform::unsupported_command("attest", "TEE attestation channel support")
            .to_string(),
    ))
}

#[cfg(not(windows))]
pub async fn execute(args: AttestArgs) -> Result<(), BoxError> {
    let state = StateFile::load_default()?;
    let record =
        resolve::resolve(&state, &args.r#box).map_err(super::IntoBoxError::into_box_error)?;

    // Generate or parse nonce
    let nonce_bytes = match &args.nonce {
        Some(hex_nonce) => hex_to_bytes(hex_nonce)?,
        None => generate_random_nonce(),
    };

    let attest_socket_path = crate::socket_paths::require_runtime_socket(
        record,
        crate::socket_paths::RuntimeSocket::Attest,
    )
    .map_err(BoxError::StateError)?;
    let socket_path = &attest_socket_path;

    // RA-TLS mode: verify attestation via TLS handshake
    if args.ratls {
        let policy = match &args.policy {
            Some(path) => load_attestation_policy(path)?,
            None => AttestationPolicy::default(),
        };

        let client = RaTlsAttestationClient::new(socket_path);
        let result = client.verify(policy, args.allow_simulated).await?;

        if args.quiet {
            if result.verified {
                println!("true");
            } else {
                println!("false");
                for f in &result.failures {
                    eprintln!("  {}", f);
                }
                std::process::exit(1);
            }
            return Ok(());
        }

        let output = AttestOutput {
            box_id: record.id.clone(),
            box_name: record.name.clone(),
            verified: Some(result.verified),
            platform: Some(result.platform),
            nonce: "(RA-TLS: bound to TLS public key)".to_string(),
            report_hex: None,
            failures: result.failures,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);

        if !result.verified {
            std::process::exit(1);
        }
        return Ok(());
    }

    // Non-RA-TLS modes still obtain the report over RA-TLS: the guest
    // attestation server speaks RA-TLS + framed messages (not plain HTTP) and
    // carries the signed report in its TLS certificate.
    let client = RaTlsAttestationClient::new(socket_path);
    let report = client.fetch_report(args.allow_simulated).await?;

    // Under RA-TLS the report's nonce is bound to the server's TLS public key,
    // so verification and output use that embedded nonce.
    let report_nonce: Vec<u8> = if report.report.len() >= 0x90 {
        report.report[0x50..0x90].to_vec()
    } else {
        nonce_bytes.clone()
    };

    // If --raw, output the report without verification
    if args.raw {
        let output = AttestOutput {
            box_id: record.id.clone(),
            box_name: record.name.clone(),
            verified: None,
            platform: a3s_box_runtime::tee::parse_platform_info(&report.report),
            nonce: bytes_to_hex(&report_nonce),
            report_hex: Some(bytes_to_hex(&report.report)),
            failures: vec![],
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    // Load or create verification policy
    let policy = match &args.policy {
        Some(path) => load_attestation_policy(path)?,
        None => AttestationPolicy::default(),
    };

    // Verify the report
    let result = verify_attestation(&report, &report_nonce, &policy, args.allow_simulated)?;

    if args.quiet {
        if result.verified {
            println!("true");
        } else {
            println!("false");
            for f in &result.failures {
                eprintln!("  {}", f);
            }
            std::process::exit(1);
        }
        return Ok(());
    }

    // Full JSON output
    let output = AttestOutput {
        box_id: record.id.clone(),
        box_name: record.name.clone(),
        verified: Some(result.verified),
        platform: Some(result.platform),
        nonce: bytes_to_hex(&report_nonce),
        report_hex: Some(bytes_to_hex(&report.report)),
        failures: result.failures,
    };

    println!("{}", serde_json::to_string_pretty(&output)?);

    if !result.verified {
        std::process::exit(1);
    }

    Ok(())
}

/// Generate a random 64-byte nonce.
#[cfg(any(not(windows), test))]
fn generate_random_nonce() -> Vec<u8> {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let mut nonce = vec![0u8; 64];
    rng.fill(&mut nonce[..]);
    nonce
}

/// Decode a hex string to bytes.
#[cfg(any(not(windows), test))]
fn hex_to_bytes(hex: &str) -> Result<Vec<u8>, BoxError> {
    let hex = hex.trim().trim_start_matches("0x");
    if !hex.len().is_multiple_of(2) {
        return Err(BoxError::ConfigError(
            "Hex string must have even length".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        let byte = u8::from_str_radix(&hex[i..i + 2], 16).map_err(|error| {
            BoxError::ConfigError(format!("Invalid hex at position {i}: {error}"))
        })?;
        bytes.push(byte);
    }
    Ok(bytes)
}

#[cfg(not(windows))]
fn load_attestation_policy(path: &std::path::Path) -> Result<AttestationPolicy, BoxError> {
    let data = std::fs::read_to_string(path).map_err(|error| {
        super::io_error(
            format!("Failed to read policy file {}", path.display()),
            error,
        )
    })?;
    parse_attestation_policy(path, &data)
}

#[cfg(not(windows))]
fn parse_attestation_policy(
    path: &std::path::Path,
    data: &str,
) -> Result<a3s_box_runtime::AttestationPolicy, BoxError> {
    serde_json::from_str(data).map_err(|error| {
        BoxError::ConfigError(format!(
            "Failed to parse policy file {}: {error}",
            path.display()
        ))
    })
}

/// Encode bytes as a hex string.
#[cfg(any(not(windows), test))]
fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_to_bytes() {
        assert_eq!(hex_to_bytes("0102ff").unwrap(), vec![1, 2, 255]);
        assert_eq!(hex_to_bytes("0x0102ff").unwrap(), vec![1, 2, 255]);
        assert_eq!(hex_to_bytes("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn test_hex_to_bytes_invalid() {
        assert!(hex_to_bytes("0g").is_err());
        assert!(hex_to_bytes("abc").is_err()); // odd length
    }

    #[test]
    fn odd_nonce_hex_is_a_configuration_error() {
        match hex_to_bytes("abc") {
            Err(BoxError::ConfigError(message)) => {
                assert!(message.contains("even length"), "{message}");
            }
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }

    #[test]
    fn invalid_nonce_hex_is_a_configuration_error() {
        match hex_to_bytes("0g") {
            Err(BoxError::ConfigError(message)) => {
                assert!(message.contains("Invalid hex"), "{message}");
            }
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn invalid_attestation_policy_is_a_configuration_error() {
        match parse_attestation_policy(std::path::Path::new("policy.json"), "not-json") {
            Err(BoxError::ConfigError(message)) => {
                assert!(message.contains("Failed to parse policy file"), "{message}");
                assert!(message.contains("policy.json"), "{message}");
            }
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_attest_is_a_configuration_error() {
        let error = execute(AttestArgs {
            r#box: "box".into(),
            policy: None,
            nonce: None,
            raw: false,
            allow_simulated: false,
            ratls: false,
            quiet: false,
        })
        .await
        .expect_err("Windows attest is unsupported");
        match error {
            BoxError::ConfigError(message) => {
                assert!(message.contains("not supported"), "{message}");
                assert!(message.contains("attest"), "{message}");
            }
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }

    #[test]
    fn test_bytes_to_hex() {
        assert_eq!(bytes_to_hex(&[1, 2, 255]), "0102ff");
        assert_eq!(bytes_to_hex(&[]), "");
    }

    #[test]
    fn test_generate_random_nonce() {
        let nonce = generate_random_nonce();
        assert_eq!(nonce.len(), 64);
        // Two random nonces should (almost certainly) differ
        let nonce2 = generate_random_nonce();
        assert_ne!(nonce, nonce2);
    }
}
