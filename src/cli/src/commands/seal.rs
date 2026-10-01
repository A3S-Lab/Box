//! `a3s-box seal` command — Encrypt data bound to a TEE's identity.
//!
//! Connects to a running box's RA-TLS attestation server, verifies the TEE,
//! then encrypts data using a key derived from the TEE's measurement and chip_id.
//! The sealed blob can only be decrypted by the same TEE.

use a3s_box_core::error::BoxError;
use clap::Args;

#[cfg(not(windows))]
use crate::resolve;
#[cfg(not(windows))]
use crate::state::StateFile;

#[cfg(not(windows))]
use a3s_box_runtime::{tee::AttestationPolicy, SealClient};

#[derive(Args)]
pub struct SealArgs {
    /// Box name or ID
    pub r#box: String,

    /// Data to seal (plaintext string)
    #[arg(long)]
    pub data: String,

    /// Application-specific context for key derivation (e.g., "model-weights", "api-keys")
    #[arg(long, default_value = "default")]
    pub context: String,

    /// Sealing policy: measurement-and-chip, measurement-only, chip-only
    #[arg(long, default_value = "measurement-and-chip")]
    pub policy: String,

    /// Accept simulated (non-hardware) TEE reports for development/testing
    #[arg(long)]
    pub allow_simulated: bool,

    /// Read data from a file instead of --data
    #[arg(long, conflicts_with = "data")]
    pub file: Option<String>,
}

/// JSON output for the seal command.
#[cfg(not(windows))]
#[derive(serde::Serialize)]
struct SealOutput {
    box_name: String,
    blob: String,
    context: String,
    policy: String,
}

#[cfg(windows)]
pub async fn execute(_args: SealArgs) -> Result<(), BoxError> {
    Err(BoxError::ConfigError(
        crate::platform::unsupported_command("seal", "TEE sealed-storage channel support")
            .to_string(),
    ))
}

#[cfg(not(windows))]
pub async fn execute(args: SealArgs) -> Result<(), BoxError> {
    let state = StateFile::load_default()?;
    let record =
        resolve::resolve(&state, &args.r#box).map_err(super::IntoBoxError::into_box_error)?;
    let attest_socket_path = crate::socket_paths::require_runtime_socket(
        record,
        crate::socket_paths::RuntimeSocket::Attest,
    )
    .map_err(BoxError::StateError)?;
    let socket_path = &attest_socket_path;

    let data = match &args.file {
        Some(path) => std::fs::read(path)
            .map_err(|error| super::io_error(format!("Failed to read file '{path}'"), error))?,
        None => args.data.as_bytes().to_vec(),
    };

    let policy = require_sealing_policy(&args.policy)?;

    let client = SealClient::new(socket_path);
    let result = client
        .seal(
            &data,
            &args.context,
            &policy,
            AttestationPolicy::default(),
            args.allow_simulated,
        )
        .await?;

    let output = SealOutput {
        box_name: record.name.clone(),
        blob: result.blob,
        context: result.context,
        policy: result.policy,
    };

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

#[cfg(any(not(windows), test))]
fn require_sealing_policy(policy: &str) -> Result<String, BoxError> {
    normalize_policy(policy).map_err(BoxError::ConfigError)
}

/// Normalize CLI-friendly policy names to internal format.
#[cfg(any(not(windows), test))]
fn normalize_policy(policy: &str) -> Result<String, String> {
    match policy.to_lowercase().replace('-', "").as_str() {
        "measurementandchip" => Ok("MeasurementAndChip".to_string()),
        "measurementonly" => Ok("MeasurementOnly".to_string()),
        "chiponly" => Ok("ChipOnly".to_string()),
        _ => Err(format!(
            "Invalid sealing policy '{}'. Valid: measurement-and-chip, measurement-only, chip-only",
            policy
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_policy_measurement_and_chip() {
        assert_eq!(
            normalize_policy("measurement-and-chip").unwrap(),
            "MeasurementAndChip"
        );
    }

    #[test]
    fn test_normalize_policy_measurement_only() {
        assert_eq!(
            normalize_policy("measurement-only").unwrap(),
            "MeasurementOnly"
        );
    }

    #[test]
    fn test_normalize_policy_chip_only() {
        assert_eq!(normalize_policy("chip-only").unwrap(), "ChipOnly");
    }

    #[test]
    fn test_normalize_policy_case_insensitive() {
        assert_eq!(
            normalize_policy("Measurement-And-Chip").unwrap(),
            "MeasurementAndChip"
        );
        assert_eq!(normalize_policy("CHIP-ONLY").unwrap(), "ChipOnly");
    }

    #[test]
    fn test_normalize_policy_invalid() {
        assert!(normalize_policy("invalid").is_err());
        assert!(normalize_policy("").is_err());
    }

    #[test]
    fn invalid_sealing_policy_is_a_configuration_error() {
        match require_sealing_policy("invalid") {
            Err(BoxError::ConfigError(message)) => {
                assert!(message.contains("Invalid sealing policy"), "{message}");
                assert!(message.contains("invalid"), "{message}");
            }
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_seal_is_a_configuration_error() {
        let error = execute(SealArgs {
            r#box: "box".into(),
            data: String::new(),
            context: "default".into(),
            policy: "measurement-and-chip".into(),
            allow_simulated: false,
            file: None,
        })
        .await
        .expect_err("Windows seal is unsupported");
        match error {
            BoxError::ConfigError(message) => {
                assert!(message.contains("not supported"), "{message}");
                assert!(message.contains("seal"), "{message}");
            }
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }
}
