//! Compose service teardown, rollback, and project removal.

use std::collections::BTreeSet;
use std::path::PathBuf;

use a3s_box_core::compose::ComposeConfig;
use a3s_box_runtime::NetworkStore;

use super::{ComposeDownArgs, LABEL_PROJECT, LABEL_SERVICE};
use crate::state::{BoxRecord, StateFile};
use crate::status;

// ============================================================================
// compose down
// ============================================================================

/// Snapshot of a compose service box for the `down` operation.
#[derive(Clone)]
pub(super) struct ServiceBox {
    pub(super) box_id: String,
    pub(super) svc_name: String,
    pub(super) pid: Option<u32>,
    pub(super) status: String,
    pub(super) box_dir: PathBuf,
    pub(super) exec_socket_path: PathBuf,
    pub(super) network_name: Option<String>,
    pub(super) volume_names: Vec<String>,
    pub(super) anonymous_volumes: Vec<String>,
    pub(super) stop_signal: Option<String>,
    pub(super) stop_timeout: Option<u64>,
}

impl ServiceBox {
    pub(super) fn from_record(record: &BoxRecord) -> Self {
        Self {
            box_id: record.id.clone(),
            svc_name: record
                .labels
                .get(LABEL_SERVICE)
                .cloned()
                .unwrap_or_default(),
            pid: record.pid,
            status: record.status.clone(),
            box_dir: record.box_dir.clone(),
            exec_socket_path: record.exec_socket_path.clone(),
            network_name: crate::cleanup::record_network_name(record).map(str::to_string),
            volume_names: record.volume_names.clone(),
            anonymous_volumes: record.anonymous_volumes.clone(),
            stop_signal: record.stop_signal.clone(),
            stop_timeout: record.stop_timeout,
        }
    }

    pub(super) fn is_active(&self) -> bool {
        status::is_active_status(&self.status)
    }
}

pub(super) fn cleanup_service_box(svc: &ServiceBox) {
    cleanup_partial_service_box(
        &svc.box_id,
        &svc.box_dir,
        &svc.exec_socket_path,
        svc.network_name.as_deref(),
        &svc.volume_names,
        &svc.anonymous_volumes,
    );
}

pub(super) fn cleanup_partial_service_box(
    box_id: &str,
    box_dir: &std::path::Path,
    exec_socket_path: &std::path::Path,
    network_name: Option<&str>,
    volume_names: &[String],
    anonymous_volumes: &[String],
) {
    crate::cleanup::cleanup_box_resources(box_id, volume_names, network_name);
    crate::cleanup::cleanup_anonymous_volumes(anonymous_volumes);
    // Release every rootfs provider before deleting the box dir. Linux uses an
    // overlay mount, while macOS mounts a case-sensitive APFS image at rootfs.
    // cleanup_box_resources above only detaches volumes and networking.
    a3s_box_runtime::rootfs::unmount_box_overlay(&box_dir.join("merged"));
    a3s_box_runtime::rootfs::unmount_box_rootfs(&box_dir.join("rootfs"));
    let _ = std::fs::remove_dir_all(box_dir);
    crate::cleanup::cleanup_external_socket_dir(box_dir, exec_socket_path);
}

pub(super) fn rollback_with_current(
    started_services: &[ServiceBox],
    current: ServiceBox,
) -> Vec<ServiceBox> {
    let mut rollback_services = started_services.to_vec();
    rollback_services.push(current);
    rollback_services
}

pub(super) async fn rollback_compose_up<T>(
    state: &mut StateFile,
    started_services: &[ServiceBox],
    created_networks: &[String],
    error: impl Into<Box<dyn std::error::Error>>,
) -> Result<T, Box<dyn std::error::Error>> {
    rollback_started_services(state, started_services).await;
    cleanup_created_networks(created_networks);
    Err(error.into())
}

async fn rollback_started_services(state: &mut StateFile, started_services: &[ServiceBox]) {
    if started_services.is_empty() {
        return;
    }

    eprintln!(
        "  [!] Rolling back {} started service(s)...",
        started_services.len()
    );

    for svc in started_services.iter().rev() {
        stop_service_process(svc).await;

        match StateFile::remove_record(&svc.box_id) {
            Ok(_) => state.forget(&svc.box_id),
            Err(error) => eprintln!(
                "  Warning: failed to remove rolled-back service {} from state: {}",
                svc.svc_name, error
            ),
        }
        cleanup_service_box(svc);
    }
}

pub(super) async fn stop_service_process(svc: &ServiceBox) {
    if !svc.is_active() {
        return;
    }

    let Some(pid) = svc.pid else {
        eprintln!(
            "  Warning: service {} is {} but has no recorded PID; removing stale service state.",
            svc.svc_name, svc.status
        );
        return;
    };

    if svc.status == "paused" {
        #[cfg(unix)]
        if let Err(error) = crate::process::send_signal(pid, libc::SIGCONT) {
            eprintln!(
                "  Warning: failed to resume paused service {} before stopping: {}",
                svc.svc_name, error
            );
        }
    }

    let stop_signal = svc
        .stop_signal
        .as_deref()
        .map(a3s_box_core::vmm::parse_signal_name)
        .unwrap_or(libc::SIGTERM);
    let stop_timeout = svc.stop_timeout.unwrap_or(10);
    let exec_socket = if svc.exec_socket_path.as_os_str().is_empty() {
        svc.box_dir.join("sockets").join("exec.sock")
    } else {
        svc.exec_socket_path.clone()
    };
    crate::process::graceful_stop_via_guest(pid, &exec_socket, stop_signal, stop_timeout).await;
}

fn cleanup_created_networks(created_networks: &[String]) {
    if created_networks.is_empty() {
        return;
    }

    let Ok(net_store) = NetworkStore::default_path() else {
        return;
    };

    for net_name in created_networks.iter().rev() {
        if let Ok(Some(mut net_config)) = net_store.get(net_name) {
            let endpoint_ids: Vec<_> = net_config.endpoints.keys().cloned().collect();
            for endpoint_id in endpoint_ids {
                let _ = net_config.disconnect(&endpoint_id);
            }
            let _ = net_store.update(&net_config);
        }

        if let Err(error) = net_store.remove(net_name) {
            eprintln!(
                "  Warning: failed to roll back network {}: {}",
                net_name, error
            );
        }
    }
}

/// `compose down` — Stop and remove all services, networks, and optionally volumes.
pub(super) async fn execute_down(
    project_name: &str,
    config: &ComposeConfig,
    down_args: ComposeDownArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;

    // Find all boxes belonging to this project
    let project_boxes: Vec<ServiceBox> = state
        .find_by_label(LABEL_PROJECT, project_name)
        .iter()
        .map(|r| ServiceBox::from_record(r))
        .collect();
    let network_names = project_network_names(project_name, config, &project_boxes);
    let volume_names = project_volume_names(config, &project_boxes);

    if project_boxes.is_empty() {
        println!("No services found for project '{}'.", project_name);
    } else {
        println!(
            "Stopping project '{}' ({} services)...",
            project_name,
            project_boxes.len()
        );

        // Stop in reverse order (last started = first stopped)
        for svc in project_boxes.iter().rev() {
            print!("  [-] Stopping {}...", svc.svc_name);

            stop_service_process(svc).await;
            StateFile::remove_record(&svc.box_id)?;
            state.forget(&svc.box_id);
            cleanup_service_box(svc);

            println!(" ✓");
        }
    }

    // Clean up networks
    if let Ok(net_store) = NetworkStore::default_path() {
        for network_name in network_names {
            if let Ok(Some(mut network)) = net_store.get(&network_name) {
                let ids = network.endpoints.keys().cloned().collect::<Vec<_>>();
                for id in ids {
                    network.disconnect(&id).ok();
                }
                let _ = net_store.update(&network);
                if let Err(error) = net_store.remove(&network_name) {
                    eprintln!(
                        "  Warning: failed to remove network {}: {}",
                        network_name, error
                    );
                } else {
                    println!("  [-] Network {} removed", network_name);
                }
            }
        }
    }

    // Optionally remove named volumes
    if down_args.volumes {
        let vol_store = a3s_box_runtime::volume::VolumeStore::default_path()?;
        let mut removed = 0u32;
        for volume_name in volume_names {
            match vol_store.remove(&volume_name, true) {
                Ok(_) => {
                    println!("  [-] Volume {} removed", volume_name);
                    removed += 1;
                }
                Err(error) => {
                    eprintln!(
                        "  Warning: failed to remove volume {}: {}",
                        volume_name, error
                    );
                }
            }
        }
        if removed > 0 {
            println!("  Removed {} volume(s).", removed);
        }
    }

    println!("Project '{}' stopped.", project_name);
    Ok(())
}

fn project_network_names(
    project_name: &str,
    config: &ComposeConfig,
    project_boxes: &[ServiceBox],
) -> BTreeSet<String> {
    let mut names = BTreeSet::from([format!("{project_name}_default")]);
    names.extend(
        config
            .networks
            .keys()
            .map(|network| format!("{project_name}_{network}")),
    );
    names.extend(config.services.values().flat_map(|service| {
        service
            .networks
            .names()
            .into_iter()
            .map(|network| format!("{project_name}_{network}"))
    }));
    names.extend(
        project_boxes
            .iter()
            .filter_map(|service| service.network_name.clone()),
    );
    names
}

fn project_volume_names(config: &ComposeConfig, project_boxes: &[ServiceBox]) -> BTreeSet<String> {
    let mut names = config.volumes.keys().cloned().collect::<BTreeSet<_>>();
    names.extend(
        project_boxes
            .iter()
            .flat_map(|service| service.volume_names.iter().cloned()),
    );
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service_box_with_resources(network: &str, volumes: &[&str]) -> ServiceBox {
        let mut record = crate::test_helpers::fixtures::make_record(
            "compose-id",
            "project-api",
            "stopped",
            None,
        );
        record.network_name = Some(network.to_string());
        record.volume_names = volumes.iter().map(|name| (*name).to_string()).collect();
        ServiceBox::from_record(&record)
    }

    #[test]
    fn teardown_names_are_exact_and_deduplicated() {
        let config = ComposeConfig::from_yaml_str(
            "services:\n  api:\n    image: api\n    networks: [backend]\nvolumes:\n  data:\nnetworks:\n  backend:\n  unused:\n",
        )
        .unwrap();
        let boxes = vec![service_box_with_resources(
            "project_legacy",
            &["data", "legacy", "data"],
        )];

        assert_eq!(
            project_network_names("project", &config, &boxes),
            BTreeSet::from([
                "project_backend".to_string(),
                "project_default".to_string(),
                "project_legacy".to_string(),
                "project_unused".to_string(),
            ])
        );
        assert_eq!(
            project_volume_names(&config, &boxes),
            BTreeSet::from(["data".to_string(), "legacy".to_string()])
        );
    }
}
