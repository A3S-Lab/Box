//! `a3s-box attest` command — Request and verify a TEE attestation report.
//!
//! Connects to a running box's agent socket, requests a hardware-signed
//! SNP attestation report, optionally verifies it against a policy, and
//! outputs the result as JSON.

use a3s_box_runtime::{
    AttestationClient, AttestationPolicy, AttestationRequest,
    verify_attestation,
};
use clap::Args;
use std::path::PathBuf;

use crate::resolve;
use crate::state::StateFile;

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

    /// Only output the verification result (true/false), no full report.
    #[arg(long, short)]
    pub quiet: bool,
}

/// JSON output for the attest command.
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

pub async fn execute(args: AttestArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;

    if record.status != "running" {
        return Err(format!("Box {} is not running", record.name).into());
    }

    // Generate or parse nonce
    let nonce_bytes = match &args.nonce {
        Some(hex_nonce) => hex_to_bytes(hex_nonce)?,
        None => generate_random_nonce(),
    };

    // Build attestation request
    let request = AttestationRequest {
        nonce: nonce_bytes.clone(),
        user_data: None,
    };

    // Connect to agent socket and request report
    let socket_path = &record.socket_path;
    if !socket_path.exists() {
        return Err(format!(
            "Agent socket not found for box {} at {}",
            record.name,
            socket_path.display()
        )
        .into());
    }

    let client = AttestationClient::connect(socket_path).await?;
    let report = client.get_report(&request).await?;

    // If --raw, output the report without verification
    if args.raw {
        let output = AttestOutput {
            box_id: record.id.clone(),
            box_name: record.name.clone(),
            verified: None,
            platform: a3s_box_runtime::tee::parse_platform_info(&report.report),
            nonce: bytes_to_hex(&nonce_bytes),
            report_hex: Some(bytes_to_hex(&report.report)),
            failures: vec![],
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    // Load or create verification policy
    let policy = match &args.policy {
        Some(path) => {
            let data = std::fs::read_to_string(path).map_err(|e| {
                format!("Failed to read policy file {}: {}", path.display(), e)
            })?;
            serde_json::from_str::<AttestationPolicy>(&data).map_err(|e| {
                format!("Failed to parse policy file {}: {}", path.display(), e)
            })?
        }
        None => AttestationPolicy::default(),
    };

    // Verify the report
    let result = verify_attestation(&report, &nonce_bytes, &policy)?;

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
        nonce: bytes_to_hex(&nonce_bytes),
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
fn generate_random_nonce() -> Vec<u8> {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let mut nonce = vec![0u8; 64];
    rng.fill(&mut nonce[..]);
    nonce
}

/// Decode a hex string to bytes.
fn hex_to_bytes(hex: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let hex = hex.trim().trim_start_matches("0x");
    if hex.len() % 2 != 0 {
        return Err("Hex string must have even length".into());
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        let byte = u8::from_str_radix(&hex[i..i + 2], 16)
            .map_err(|e| format!("Invalid hex at position {}: {}", i, e))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

/// Encode bytes as a hex string.
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
