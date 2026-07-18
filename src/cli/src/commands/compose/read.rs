//! Read-only Compose project views and log streaming.

use std::collections::HashMap;

use a3s_box_core::compose::ComposeConfig;
use a3s_box_runtime::ComposeProject;

use super::{
    validate_compose_restart_policies, ComposeLogsArgs, ProjectServicesArgs, LABEL_PROJECT,
    LABEL_SERVICE,
};
use crate::state::StateFile;

// ============================================================================
// compose ps
// ============================================================================

/// `compose ps` — List services and their actual status.
pub(super) async fn execute_ps(
    project_name: &str,
    config: &ComposeConfig,
    args: ProjectServicesArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let boxes = state
        .find_by_label(LABEL_PROJECT, project_name)
        .into_iter()
        .filter(|record| {
            args.services.is_empty()
                || record
                    .labels
                    .get(LABEL_SERVICE)
                    .is_some_and(|service| args.services.contains(service))
        })
        .collect::<Vec<_>>();

    for service in &args.services {
        if !config.services.contains_key(service) {
            return Err(
                format!("Service '{service}' is not defined in project '{project_name}'.").into(),
            );
        }
    }

    if boxes.is_empty() {
        println!("No services found for project '{}'.", project_name);
        return Ok(());
    }

    println!(
        "{:<20} {:<30} {:<12} {:<12} {:<10}",
        "SERVICE", "IMAGE", "STATUS", "HEALTH", "PID"
    );
    println!("{}", "-".repeat(84));

    for record in &boxes {
        let svc_name = record
            .labels
            .get(LABEL_SERVICE)
            .map(|s| s.as_str())
            .unwrap_or("?");
        let pid_str = record
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<20} {:<30} {:<12} {:<12} {:<10}",
            svc_name, record.image, record.status, record.health_status, pid_str
        );
    }

    Ok(())
}

// ============================================================================
// compose config
// ============================================================================

/// `compose config` — Validate and display the parsed compose configuration.
pub(super) fn execute_config(
    project_name: &str,
    config: ComposeConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_compose_restart_policies(&config)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let project = ComposeProject::new(project_name, config)?;

    println!("Project: {}", project_name);
    println!("Services: {}", project.config.services.len());
    println!("Networks: {}", project.required_networks().len());
    println!("Volumes: {}", project.config.volumes.len());
    println!("\nBoot order: {}", project.service_order.join(" → "));

    for svc_name in &project.service_order {
        if let Some(svc) = project.config.services.get(svc_name) {
            println!("\n[{}]", svc_name);
            if let Some(ref img) = svc.image {
                println!("  image: {}", img);
            }
            if !svc.ports.is_empty() {
                println!("  ports: {}", svc.ports.join(", "));
            }
            if !svc.volumes.is_empty() {
                println!("  volumes: {}", svc.volumes.join(", "));
            }
            let deps = svc.depends_on.services();
            if !deps.is_empty() {
                println!("  depends_on: {}", deps.join(", "));
            }
            let env = svc.environment.to_pairs();
            if !env.is_empty() {
                println!("  environment:");
                for (k, v) in &env {
                    println!("    {}={}", k, v);
                }
            }
        }
    }

    println!("\nConfiguration is valid.");
    Ok(())
}

// ============================================================================
// compose logs
// ============================================================================

/// `compose logs` — View logs from all (or one) service in the project.
pub(super) async fn execute_logs(
    project_name: &str,
    config: &ComposeConfig,
    logs_args: ComposeLogsArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    if !logs_args.services.is_empty() {
        super::operations::selected_service_names(config, &logs_args.services)?;
    }
    let state = StateFile::load_default()?;
    let boxes = state.find_by_label(LABEL_PROJECT, project_name);

    if boxes.is_empty() {
        println!("No services found for project '{}'.", project_name);
        return Ok(());
    }

    // Filter to requested services if supplied.
    let targets: Vec<_> = if !logs_args.services.is_empty() {
        boxes
            .iter()
            .filter(|r| {
                r.labels
                    .get(LABEL_SERVICE)
                    .is_some_and(|service| logs_args.services.contains(service))
            })
            .collect()
    } else {
        boxes.iter().collect()
    };

    if targets.is_empty() && !logs_args.services.is_empty() {
        return Err(format!(
            "Services '{}' were not found in project '{}'.",
            logs_args.services.join(", "),
            project_name
        )
        .into());
    }

    for record in &targets {
        let svc_name = record
            .labels
            .get(LABEL_SERVICE)
            .map(|s| s.as_str())
            .unwrap_or("?");

        let log_path = record.console_log.clone();
        if !log_path.exists() {
            println!("[{}] (no logs)", svc_name);
            continue;
        }

        let content = std::fs::read_to_string(&log_path)
            .map_err(|e| format!("Failed to read logs for {}: {}", svc_name, e))?;

        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(logs_args.tail);
        let prefix = if targets.len() > 1 {
            format!("{} | ", svc_name)
        } else {
            String::new()
        };

        for line in &lines[start..] {
            println!("{}{}", prefix, line);
        }
    }

    if logs_args.follow {
        println!("(follow mode: use Ctrl-C to stop)");
        // In follow mode, tail all log files concurrently
        // For simplicity, we poll every second
        let mut last_sizes: HashMap<String, u64> = HashMap::new();
        for record in &targets {
            let size = std::fs::metadata(&record.console_log)
                .map(|m| m.len())
                .unwrap_or(0);
            last_sizes.insert(record.id.clone(), size);
        }

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            for record in &targets {
                let log_path = &record.console_log;
                let current_size = std::fs::metadata(log_path).map(|m| m.len()).unwrap_or(0);
                let last_size = last_sizes.get(&record.id).copied().unwrap_or(0);

                if current_size > last_size {
                    let svc_name = record
                        .labels
                        .get(LABEL_SERVICE)
                        .map(|s| s.as_str())
                        .unwrap_or("?");
                    let prefix = if targets.len() > 1 {
                        format!("{} | ", svc_name)
                    } else {
                        String::new()
                    };

                    if let Ok(file) = std::fs::File::open(log_path) {
                        use std::io::{Read, Seek, SeekFrom};
                        let mut file = file;
                        if file.seek(SeekFrom::Start(last_size)).is_ok() {
                            let mut buf = String::new();
                            if file.read_to_string(&mut buf).is_ok() {
                                for line in buf.lines() {
                                    println!("{}{}", prefix, line);
                                }
                            }
                        }
                    }

                    last_sizes.insert(record.id.clone(), current_size);
                }
            }
        }
    }

    Ok(())
}
