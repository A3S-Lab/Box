//! `a3s-box start` command — Start one or more eligible boxes.

use a3s_box_core::{ExecutionGeneration, ExecutionId, ExecutionManager};
use a3s_box_runtime::{LocalExecutionManager, ManagedExecutionState};
use clap::Args;

use crate::boot;
use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct StartArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,
}

pub async fn execute(args: StartArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = start_one(&state, query).await {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

async fn start_one(state: &StateFile, query: &str) -> Result<(), Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?;
    let plan =
        start_plan(record).map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;

    let box_id = record.id.clone();
    let name = record.name.clone();

    println!("Starting box {name}...");
    let started_record = match plan {
        StartPlan::Legacy => {
            let result = boot::boot_from_record(record).await?;

            // Persist the boot result atomically (load-fresh + mutate + save under the
            // state lock) so it cannot clobber a concurrent writer with our pre-boot
            // snapshot.
            StateFile::modify({
                let box_id = box_id.clone();
                move |state| {
                    Ok::<_, std::io::Error>(state.find_by_id_mut(&box_id).map(|record| {
                        boot::apply_boot_result(record, result, boot::RestartCountUpdate::Reset);
                        record.clone()
                    }))
                }
            })?
        }
        StartPlan::Managed {
            execution_id,
            generation,
        } => {
            let home = a3s_box_core::dirs_home();
            let manager = LocalExecutionManager::with_vm_backend(home.join("boxes.json"), &home);
            manager.start(&execution_id, generation).await?;

            let baseline_box_dir = record.box_dir.clone();
            let baseline_box_id = record.id.clone();
            match tokio::task::spawn_blocking(move || {
                crate::commands::diff::create_box_baseline_snapshot(&baseline_box_dir)
                    .map_err(|error| error.to_string())
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(
                        box_id = %baseline_box_id,
                        %error,
                        "Failed to create rootfs diff baseline snapshot"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        box_id = %baseline_box_id,
                        %error,
                        "Rootfs diff baseline task failed"
                    );
                }
            }

            StateFile::load_default()?.find_by_id(&box_id).cloned()
        }
    };
    if let Some(record) = started_record {
        crate::health::spawn_detached_health_checker(&record)
            .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    }

    println!("{name}");
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StartPlan {
    Legacy,
    Managed {
        execution_id: ExecutionId,
        generation: ExecutionGeneration,
    },
}

fn start_plan(record: &crate::state::BoxRecord) -> Result<StartPlan, String> {
    let Some(metadata) = record.managed_execution.as_ref() else {
        validate_start_status(&record.name, &record.status)?;
        return Ok(StartPlan::Legacy);
    };
    let state = record
        .managed_state()
        .map_err(|error| format!("Invalid managed state for box {}: {error}", record.name))?
        .ok_or_else(|| format!("Box {} lost managed lifecycle metadata", record.name))?;

    match state {
        ManagedExecutionState::Creating
        | ManagedExecutionState::Created
        | ManagedExecutionState::Starting => Ok(StartPlan::Managed {
            execution_id: ExecutionId::new(record.id.clone()).map_err(|error| error.to_string())?,
            generation: metadata.generation,
        }),
        ManagedExecutionState::Running => Err(format!("Box {} is already running", record.name)),
        ManagedExecutionState::Stopped | ManagedExecutionState::Failed => Err(format!(
            "Box {} is {state}; ordinary start cannot revive a terminal managed execution without advancing its generation",
            record.name
        )),
        other => Err(format!("Cannot start box in state: {other}")),
    }
}

fn validate_start_status(name: &str, status: &str) -> Result<(), String> {
    match status {
        "created" | "stopped" | "dead" => Ok(()),
        "running" => Err(format!("Box {name} is already running")),
        other => Err(format!("Cannot start box in state: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::{BoxConfig, CreateExecutionRequest, ExecutionIsolation, OperationId};
    use a3s_box_runtime::{ManagedExecutionMetadata, ManagedExecutionOperation};
    use std::collections::BTreeMap;

    use crate::test_helpers::fixtures::make_record;

    fn managed_record(status: ManagedExecutionState) -> crate::state::BoxRecord {
        let id = "11111111-1111-4111-8111-111111111111";
        let mut record = make_record(id, "web", status.as_status(), None);
        record.isolation = ExecutionIsolation::Sandbox;
        let mut metadata = ManagedExecutionMetadata::new(
            OperationId::new("operation-1").unwrap(),
            ExecutionGeneration::INITIAL,
            CreateExecutionRequest {
                external_sandbox_id: "external-1".to_string(),
                config: BoxConfig {
                    isolation: ExecutionIsolation::Sandbox,
                    image: record.image.clone(),
                    ..Default::default()
                },
                labels: BTreeMap::new(),
                policy: Default::default(),
            },
        )
        .unwrap();
        if status == ManagedExecutionState::Starting {
            metadata.pending_operation = Some(ManagedExecutionOperation::Start);
        }
        record.managed_execution = Some(metadata);
        record
    }

    #[test]
    fn validate_start_status_accepts_startable_states() {
        assert!(validate_start_status("web", "created").is_ok());
        assert!(validate_start_status("web", "stopped").is_ok());
        assert!(validate_start_status("web", "dead").is_ok());
    }

    #[test]
    fn validate_start_status_rejects_running_box_by_name() {
        assert_eq!(
            validate_start_status("web", "running").unwrap_err(),
            "Box web is already running"
        );
    }

    #[test]
    fn validate_start_status_rejects_other_states() {
        assert_eq!(
            validate_start_status("web", "paused").unwrap_err(),
            "Cannot start box in state: paused"
        );
    }

    #[test]
    fn managed_start_plan_uses_the_persisted_generation() {
        for status in [
            ManagedExecutionState::Creating,
            ManagedExecutionState::Created,
            ManagedExecutionState::Starting,
        ] {
            assert_eq!(
                start_plan(&managed_record(status)).unwrap(),
                StartPlan::Managed {
                    execution_id: ExecutionId::new("11111111-1111-4111-8111-111111111111").unwrap(),
                    generation: ExecutionGeneration::INITIAL,
                }
            );
        }
    }

    #[test]
    fn managed_start_plan_rejects_terminal_resurrection() {
        for status in [
            ManagedExecutionState::Stopped,
            ManagedExecutionState::Failed,
        ] {
            let error = start_plan(&managed_record(status)).unwrap_err();
            assert!(error.contains("cannot revive a terminal managed execution"));
            assert!(error.contains("advancing its generation"));
        }
    }

    #[test]
    fn legacy_stopped_box_keeps_legacy_start_behavior() {
        assert_eq!(
            start_plan(&make_record("legacy", "legacy", "stopped", None)).unwrap(),
            StartPlan::Legacy
        );
    }
}
