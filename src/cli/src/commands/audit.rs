//! `a3s-box audit` command — View the audit log.
//!
//! Reads persistent audit events with optional filters.

use a3s_box_core::audit::{AuditAction, AuditOutcome};
use a3s_box_core::error::BoxError;
use a3s_box_runtime::{read_audit_log, AuditLog, AuditQuery};
use clap::Args;

#[derive(Args)]
pub struct AuditArgs {
    /// Filter by action (e.g., "box_create", "exec_command", "image_pull")
    #[arg(long)]
    pub action: Option<String>,

    /// Filter by box ID
    #[arg(long)]
    pub box_id: Option<String>,

    /// Filter by outcome: success, failure, denied
    #[arg(long)]
    pub outcome: Option<String>,

    /// Maximum number of events to show
    #[arg(short = 'n', long, default_value = "50")]
    pub limit: usize,

    /// Output as raw JSON lines
    #[arg(long)]
    pub json: bool,
}

pub async fn execute(args: AuditArgs) -> Result<(), BoxError> {
    let audit_log = AuditLog::default_path()?;
    let path = audit_log.path();

    let action = match &args.action {
        Some(action) => Some(parse_action_filter(action)?),
        None => None,
    };
    let outcome = match &args.outcome {
        Some(outcome) => Some(parse_outcome_filter(outcome)?),
        None => None,
    };

    let query = AuditQuery {
        action,
        box_id: args.box_id.clone(),
        outcome,
        limit: Some(args.limit),
        ..Default::default()
    };

    let events = read_audit_log(&path, &query)?;

    if events.is_empty() {
        println!("No audit events found.");
        return Ok(());
    }

    if args.json {
        for event in &events {
            println!("{}", serde_json::to_string(event)?);
        }
    } else {
        println!(
            "{:<24} {:<18} {:<12} {:<10} MESSAGE",
            "TIMESTAMP", "ACTION", "BOX", "OUTCOME"
        );
        println!("{}", "-".repeat(80));

        for event in &events {
            let ts = event.timestamp.format("%Y-%m-%d %H:%M:%S");
            let action = serde_json::to_string(&event.action)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string();
            let box_id = event
                .box_id
                .as_deref()
                .map(|id| if id.len() > 10 { &id[..10] } else { id })
                .unwrap_or("-");
            let outcome = serde_json::to_string(&event.outcome)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string();
            let message = event.message.as_deref().unwrap_or("");

            println!(
                "{:<24} {:<18} {:<12} {:<10} {}",
                ts, action, box_id, outcome, message
            );
        }

        println!("\n{} event(s)", events.len());
    }

    Ok(())
}

fn parse_action_filter(action: &str) -> Result<AuditAction, BoxError> {
    serde_json::from_str(&format!("\"{action}\"")).map_err(|_| {
        BoxError::ConfigError(format!(
            "Unknown action '{action}'. Examples: box_create, exec_command, image_pull"
        ))
    })
}

fn parse_outcome_filter(outcome: &str) -> Result<AuditOutcome, BoxError> {
    serde_json::from_str(&format!("\"{outcome}\"")).map_err(|_| {
        BoxError::ConfigError(format!(
            "Unknown outcome '{outcome}'. Use: success, failure, denied"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_action_filter() {
        let parsed: AuditAction = serde_json::from_str("\"box_create\"").unwrap();
        assert_eq!(parsed, AuditAction::BoxCreate);
    }

    #[test]
    fn test_parse_outcome_filter() {
        let parsed: AuditOutcome = serde_json::from_str("\"success\"").unwrap();
        assert_eq!(parsed, AuditOutcome::Success);
    }

    #[test]
    fn test_parse_invalid_action() {
        let result: Result<AuditAction, _> = serde_json::from_str("\"nonexistent\"");
        assert!(result.is_err());
    }

    #[test]
    fn unknown_action_is_a_configuration_error() {
        match parse_action_filter("nonexistent") {
            Err(BoxError::ConfigError(message)) => {
                assert!(message.contains("Unknown action"), "{message}");
                assert!(message.contains("nonexistent"), "{message}");
            }
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }

    #[test]
    fn unknown_outcome_is_a_configuration_error() {
        match parse_outcome_filter("maybe") {
            Err(BoxError::ConfigError(message)) => {
                assert!(message.contains("Unknown outcome"), "{message}");
                assert!(message.contains("maybe"), "{message}");
            }
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }
}
