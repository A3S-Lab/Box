//! Project-scoped Compose operations built from the canonical box commands.

use std::collections::{BTreeMap, BTreeSet};

use a3s_box_core::compose::ComposeConfig;
use clap::Args;

use super::{LABEL_PROJECT, LABEL_SERVICE};
use crate::state::{BoxRecord, StateFile};

pub(super) fn select_up_config(
    mut config: ComposeConfig,
    requested: &[String],
) -> Result<ComposeConfig, Box<dyn std::error::Error>> {
    if requested.is_empty() {
        config.service_order()?;
        return Ok(config);
    }

    let mut selected = BTreeSet::new();
    for service in requested {
        collect_service_dependencies(&config, service, &mut selected)?;
    }
    config.services.retain(|name, _| selected.contains(name));
    config.service_order()?;
    Ok(config)
}

fn collect_service_dependencies(
    config: &ComposeConfig,
    service: &str,
    selected: &mut BTreeSet<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let definition = config
        .services
        .get(service)
        .ok_or_else(|| format!("service '{service}' is not defined in the Compose project"))?;
    if !selected.insert(service.to_string()) {
        return Ok(());
    }
    for dependency in definition.depends_on.services() {
        collect_service_dependencies(config, &dependency, selected)?;
    }
    Ok(())
}

#[derive(Args)]
pub struct ProjectServicesArgs {
    /// Limit the operation to these services (default: every project service)
    #[arg(value_name = "SERVICE")]
    pub services: Vec<String>,
}

#[derive(Args)]
pub struct ComposeStopArgs {
    /// Seconds to wait before force-killing
    #[arg(short = 't', long)]
    pub timeout: Option<u64>,

    /// Limit the operation to these services
    #[arg(value_name = "SERVICE")]
    pub services: Vec<String>,
}

#[derive(Args)]
pub struct ComposeRestartArgs {
    /// Seconds to wait before force-killing
    #[arg(short = 't', long, default_value_t = 10)]
    pub timeout: u64,

    /// Limit the operation to these services
    #[arg(value_name = "SERVICE")]
    pub services: Vec<String>,
}

#[derive(Args)]
pub struct ComposeRmArgs {
    /// Stop active services before removing them
    #[arg(short = 's', long)]
    pub stop: bool,

    /// Do not ask for confirmation (A3S Box is non-interactive by default)
    #[arg(short = 'f', long)]
    pub force: bool,

    /// Limit the operation to these services
    #[arg(value_name = "SERVICE")]
    pub services: Vec<String>,
}

#[derive(Args)]
pub struct ComposeKillArgs {
    /// Signal to send
    #[arg(short = 's', long, default_value = "KILL")]
    pub signal: String,

    /// Limit the operation to these services
    #[arg(value_name = "SERVICE")]
    pub services: Vec<String>,
}

#[derive(Args)]
pub struct ComposeWaitArgs {
    /// Seconds between keepalive messages (0 disables them)
    #[arg(long, default_value_t = 60)]
    pub heartbeat_interval: u64,

    /// Disable keepalive messages
    #[arg(long)]
    pub no_heartbeat: bool,

    /// Limit the operation to these services
    #[arg(value_name = "SERVICE")]
    pub services: Vec<String>,
}

#[derive(Args)]
pub struct ComposeExecArgs {
    /// Service name
    pub service: String,

    /// Timeout in seconds
    #[arg(long, default_value_t = 5)]
    pub timeout: u64,

    /// Set an environment variable (KEY=VALUE)
    #[arg(short, long = "env")]
    pub envs: Vec<String>,

    /// Working directory inside the service box
    #[arg(short, long)]
    pub workdir: Option<String>,

    /// Keep standard input open
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Allocate a pseudo-terminal
    #[arg(short = 't', long = "tty")]
    pub tty: bool,

    /// Run as a specific user
    #[arg(short = 'u', long)]
    pub user: Option<String>,

    /// Command and arguments
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

#[derive(Args)]
pub struct ComposeTopArgs {
    /// Limit output to these services
    #[arg(value_name = "SERVICE")]
    pub services: Vec<String>,
}

#[derive(Args)]
pub struct ComposePortArgs {
    /// Service name
    pub service: String,

    /// Private port and optional protocol, for example 80 or 80/tcp
    pub private_port: Option<String>,
}

#[derive(Args)]
pub struct ComposeCpArgs {
    /// Source path (HOST_PATH or SERVICE:CONTAINER_PATH)
    pub src: String,

    /// Destination path (HOST_PATH or SERVICE:CONTAINER_PATH)
    pub dst: String,
}

#[derive(Args)]
pub struct ComposePullArgs {
    /// Suppress progress output
    #[arg(short, long)]
    pub quiet: bool,

    /// Target platform, for example linux/amd64
    #[arg(long)]
    pub platform: Option<String>,

    /// Limit the operation to these services
    #[arg(value_name = "SERVICE")]
    pub services: Vec<String>,
}

#[derive(Args)]
pub struct ComposeLsArgs {
    /// Print project names only
    #[arg(short, long)]
    pub quiet: bool,
}

#[derive(Clone)]
struct ProjectBox {
    id: String,
    name: String,
    service: String,
    status: String,
    record: BoxRecord,
}

pub async fn execute_start(
    project_name: &str,
    config: &ComposeConfig,
    args: ProjectServicesArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let boxes = select_boxes(project_name, config, &args.services, false)?;
    let queries = matching_ids(&boxes, |status| {
        matches!(status, "created" | "stopped" | "dead")
    });
    if queries.is_empty() {
        println!("All selected services are already active.");
        return Ok(());
    }
    super::super::start::execute(super::super::start::StartArgs { boxes: queries }).await
}

pub async fn execute_stop(
    project_name: &str,
    config: &ComposeConfig,
    args: ComposeStopArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let boxes = select_boxes(project_name, config, &args.services, true)?;
    let queries = matching_ids(&boxes, |status| matches!(status, "running" | "paused"));
    if queries.is_empty() {
        println!("All selected services are already stopped.");
        return Ok(());
    }
    super::super::stop::execute(super::super::stop::StopArgs {
        boxes: queries,
        timeout: args.timeout,
    })
    .await
}

pub async fn execute_restart(
    project_name: &str,
    config: &ComposeConfig,
    args: ComposeRestartArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let boxes = select_boxes(project_name, config, &args.services, false)?;
    super::super::restart::execute(super::super::restart::RestartArgs {
        boxes: ids(&boxes),
        timeout: args.timeout,
    })
    .await
}

pub async fn execute_rm(
    project_name: &str,
    config: &ComposeConfig,
    args: ComposeRmArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let boxes = select_boxes(project_name, config, &args.services, true)?;
    let active = matching_ids(&boxes, |status| matches!(status, "running" | "paused"));
    if !active.is_empty() {
        if !args.stop {
            return Err(
                "selected services are active; pass --stop or run `compose stop` first".into(),
            );
        }
        super::super::stop::execute(super::super::stop::StopArgs {
            boxes: active,
            timeout: None,
        })
        .await?;
    }
    let _confirmation_is_implicit = args.force;
    super::super::rm::execute(super::super::rm::RmArgs {
        boxes: ids(&boxes),
        force: false,
    })
    .await
}

pub async fn execute_kill(
    project_name: &str,
    config: &ComposeConfig,
    args: ComposeKillArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let boxes = select_boxes(project_name, config, &args.services, true)?;
    let queries = matching_ids(&boxes, |status| matches!(status, "running" | "paused"));
    if queries.is_empty() {
        println!("No selected services are active.");
        return Ok(());
    }
    super::super::kill::execute(super::super::kill::KillArgs {
        boxes: queries,
        signal: args.signal,
    })
    .await
}

pub async fn execute_pause(
    project_name: &str,
    config: &ComposeConfig,
    args: ProjectServicesArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let boxes = select_boxes(project_name, config, &args.services, false)?;
    let queries = matching_ids(&boxes, |status| status == "running");
    if queries.is_empty() {
        println!("No selected services are running.");
        return Ok(());
    }
    super::super::pause::execute(super::super::pause::PauseArgs { boxes: queries }).await
}

pub async fn execute_unpause(
    project_name: &str,
    config: &ComposeConfig,
    args: ProjectServicesArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let boxes = select_boxes(project_name, config, &args.services, false)?;
    let queries = matching_ids(&boxes, |status| status == "paused");
    if queries.is_empty() {
        println!("No selected services are paused.");
        return Ok(());
    }
    super::super::unpause::execute(super::super::unpause::UnpauseArgs { boxes: queries }).await
}

pub async fn execute_wait(
    project_name: &str,
    config: &ComposeConfig,
    args: ComposeWaitArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let boxes = select_boxes(project_name, config, &args.services, false)?;
    super::super::wait::execute(super::super::wait::WaitArgs {
        boxes: ids(&boxes),
        heartbeat_interval: args.heartbeat_interval,
        no_heartbeat: args.no_heartbeat,
    })
    .await
}

pub async fn execute_exec(
    project_name: &str,
    config: &ComposeConfig,
    args: ComposeExecArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let service_box = one_service(project_name, config, &args.service)?;
    super::super::exec::execute(super::super::exec::ExecArgs {
        r#box: service_box.id,
        timeout: args.timeout,
        envs: args.envs,
        workdir: args.workdir,
        interactive: args.interactive,
        tty: args.tty,
        user: args.user,
        cmd: args.command,
    })
    .await
}

pub async fn execute_top(
    project_name: &str,
    config: &ComposeConfig,
    args: ComposeTopArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let boxes = select_boxes(project_name, config, &args.services, false)?;
    for service_box in boxes {
        println!("{}", service_box.service);
        super::super::top::execute(super::super::top::TopArgs {
            r#box: service_box.id,
            format: super::super::top::TopFormat::Table,
            ps_args: Vec::new(),
        })
        .await?;
    }
    Ok(())
}

pub async fn execute_port(
    project_name: &str,
    config: &ComposeConfig,
    args: ComposePortArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let service_box = one_service(project_name, config, &args.service)?;
    let requested = args.private_port.as_deref().map(normalize_private_port);
    let mut found = false;
    for value in &service_box.record.port_map {
        let mapping = a3s_box_core::parse_port_mapping(value)?;
        let private = format!("{}/{}", mapping.guest_port, mapping.protocol.as_str());
        if requested.as_deref().is_some_and(|value| value != private) {
            continue;
        }
        found = true;
        println!("0.0.0.0:{}", mapping.host_port);
    }
    if requested.is_some() && !found {
        return Err(format!(
            "service '{}' does not publish the requested port",
            args.service
        )
        .into());
    }
    Ok(())
}

pub async fn execute_cp(
    project_name: &str,
    config: &ComposeConfig,
    args: ComposeCpArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let src = resolve_copy_endpoint(project_name, config, args.src)?;
    let dst = resolve_copy_endpoint(project_name, config, args.dst)?;
    super::super::cp::execute(super::super::cp::CpArgs { src, dst }).await
}

pub fn execute_images(
    _project_name: &str,
    config: &ComposeConfig,
    args: ProjectServicesArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let services = selected_service_names(config, &args.services)?;
    let mut table = crate::output::new_table(&["SERVICE", "IMAGE"]);
    for service in services {
        let image = config
            .services
            .get(&service)
            .and_then(|value| value.image.as_deref())
            .unwrap_or("<build-only>");
        table.add_row([service.as_str(), image]);
    }
    println!("{table}");
    Ok(())
}

pub async fn execute_pull(
    _project_name: &str,
    config: &ComposeConfig,
    args: ComposePullArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let services = selected_service_names(config, &args.services)?;
    let mut images = BTreeSet::new();
    for service in services {
        if let Some(image) = config
            .services
            .get(&service)
            .and_then(|value| value.image.clone())
        {
            images.insert(image);
        }
    }
    if images.is_empty() {
        return Err("selected services do not declare an image".into());
    }
    for image in images {
        super::super::pull::execute(super::super::pull::PullArgs {
            image,
            quiet: args.quiet,
            platform: args.platform.clone(),
            verify_key: None,
            verify_issuer: None,
            verify_identity: None,
        })
        .await?;
    }
    Ok(())
}

pub async fn execute_ls(args: ComposeLsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let mut projects: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for record in state.records() {
        let Some(project) = record.labels.get(LABEL_PROJECT) else {
            continue;
        };
        let entry = projects.entry(project.clone()).or_default();
        entry.0 += 1;
        if matches!(record.status.as_str(), "running" | "paused") {
            entry.1 += 1;
        }
    }
    if args.quiet {
        for project in projects.keys() {
            println!("{project}");
        }
        return Ok(());
    }
    let mut table = crate::output::new_table(&["NAME", "STATUS", "SERVICES"]);
    for (project, (total, active)) in projects {
        let status = if active == total {
            "running"
        } else if active == 0 {
            "stopped"
        } else {
            "partial"
        };
        table.add_row([project, status.to_string(), format!("{active}/{total}")]);
    }
    println!("{table}");
    Ok(())
}

pub fn execute_volumes(
    _project_name: &str,
    config: &ComposeConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut volumes = config.volumes.keys().cloned().collect::<Vec<_>>();
    volumes.sort();
    for volume in volumes {
        println!("{volume}");
    }
    Ok(())
}

fn select_boxes(
    project_name: &str,
    config: &ComposeConfig,
    requested: &[String],
    reverse: bool,
) -> Result<Vec<ProjectBox>, Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let mut by_service: BTreeMap<String, Vec<ProjectBox>> = BTreeMap::new();
    for record in state.find_by_label(LABEL_PROJECT, project_name) {
        let Some(service) = record.labels.get(LABEL_SERVICE) else {
            continue;
        };
        by_service
            .entry(service.clone())
            .or_default()
            .push(ProjectBox {
                id: record.id.clone(),
                name: record.name.clone(),
                service: service.clone(),
                status: record.status.clone(),
                record: record.clone(),
            });
    }
    if by_service.is_empty() {
        return Err(format!(
            "No services found for project '{project_name}'. Run `compose up` first."
        )
        .into());
    }

    let existing = by_service.keys().cloned().collect::<BTreeSet<_>>();
    let service_order = service_box_order(config, &existing, requested, reverse)?;

    let mut result = Vec::new();
    for service in service_order {
        let Some(mut boxes) = by_service.remove(&service) else {
            if !requested.is_empty() {
                return Err(format!("service '{service}' has not been created").into());
            }
            continue;
        };
        boxes.sort_by(|left, right| left.name.cmp(&right.name));
        result.extend(boxes);
    }
    Ok(result)
}

fn service_box_order(
    config: &ComposeConfig,
    existing: &BTreeSet<String>,
    requested: &[String],
    reverse: bool,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut service_order = if requested.is_empty() {
        let mut order = config.service_order()?;
        for service in existing {
            if !order.contains(service) {
                order.push(service.clone());
            }
        }
        order
    } else {
        validate_requested_services(config, existing, requested)?;
        unique_service_names(requested)
    };
    if reverse {
        service_order.reverse();
    }
    Ok(service_order)
}

fn validate_requested_services(
    config: &ComposeConfig,
    existing: &BTreeSet<String>,
    requested: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    for service in requested {
        if !config.services.contains_key(service) && !existing.contains(service) {
            return Err(
                format!("service '{service}' is not defined in the Compose project").into(),
            );
        }
    }
    Ok(())
}

pub(super) fn selected_service_names(
    config: &ComposeConfig,
    requested: &[String],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    if requested.is_empty() {
        return Ok(config.service_order()?);
    }
    for service in requested {
        if !config.services.contains_key(service) {
            return Err(
                format!("service '{service}' is not defined in the Compose project").into(),
            );
        }
    }
    Ok(unique_service_names(requested))
}

fn unique_service_names(requested: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    requested
        .iter()
        .filter(|service| seen.insert((*service).clone()))
        .cloned()
        .collect()
}

fn one_service(
    project_name: &str,
    config: &ComposeConfig,
    service: &str,
) -> Result<ProjectBox, Box<dyn std::error::Error>> {
    let boxes = select_boxes(project_name, config, &[service.to_string()], false)?;
    if boxes.len() != 1 {
        return Err(format!(
            "service '{service}' resolves to {} boxes; select one instance explicitly",
            boxes.len()
        )
        .into());
    }
    boxes
        .into_iter()
        .next()
        .ok_or_else(|| format!("service '{service}' did not resolve to a box").into())
}

fn resolve_copy_endpoint(
    project_name: &str,
    config: &ComposeConfig,
    endpoint: String,
) -> Result<String, Box<dyn std::error::Error>> {
    let Some((service, path)) = endpoint.split_once(':') else {
        return Ok(endpoint);
    };
    if service.len() == 1 {
        return Ok(endpoint);
    }
    if !config.services.contains_key(service) {
        return Err(format!("service '{service}' is not defined in the Compose project").into());
    }
    let service_box = one_service(project_name, config, service)?;
    Ok(format!("{}:{path}", service_box.id))
}

fn normalize_private_port(value: &str) -> String {
    if value.contains('/') {
        value.to_string()
    } else {
        format!("{value}/tcp")
    }
}

fn ids(boxes: &[ProjectBox]) -> Vec<String> {
    boxes.iter().map(|value| value.id.clone()).collect()
}

fn matching_ids(boxes: &[ProjectBox], predicate: impl Fn(&str) -> bool) -> Vec<String> {
    boxes
        .iter()
        .filter(|value| predicate(&value.status))
        .map(|value| value.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ports_default_to_tcp() {
        assert_eq!(normalize_private_port("8080"), "8080/tcp");
        assert_eq!(normalize_private_port("53/udp"), "53/udp");
    }

    #[test]
    fn service_selection_rejects_unknown_names() {
        let config = ComposeConfig::from_yaml_str("services:\n  web:\n    image: nginx\n").unwrap();
        let error = selected_service_names(&config, &["db".to_string()]).unwrap_err();
        assert!(error.to_string().contains("service 'db' is not defined"));
    }

    #[test]
    fn up_selection_includes_transitive_dependencies_only() {
        let config = ComposeConfig::from_yaml_str(
            "services:\n  db:\n    image: postgres\n  api:\n    image: api\n    depends_on: [db]\n  worker:\n    image: worker\n",
        )
        .unwrap();

        let selected = select_up_config(config, &["api".to_string()]).unwrap();

        assert!(selected.services.contains_key("api"));
        assert!(selected.services.contains_key("db"));
        assert!(!selected.services.contains_key("worker"));
    }

    #[test]
    fn explicit_box_selection_does_not_include_unrequested_services() {
        let config = ComposeConfig::from_yaml_str(
            "services:\n  db:\n    image: postgres\n  api:\n    image: api\n    depends_on: [db]\n",
        )
        .unwrap();
        let existing = BTreeSet::from(["api".to_string(), "db".to_string(), "orphan".to_string()]);

        let selected = service_box_order(&config, &existing, &["api".to_string()], false).unwrap();

        assert_eq!(selected, ["api"]);
    }

    #[test]
    fn implicit_box_selection_keeps_config_order_and_existing_orphans() {
        let config = ComposeConfig::from_yaml_str(
            "services:\n  db:\n    image: postgres\n  api:\n    image: api\n    depends_on: [db]\n",
        )
        .unwrap();
        let existing = BTreeSet::from(["api".to_string(), "db".to_string(), "orphan".to_string()]);

        let selected = service_box_order(&config, &existing, &[], false).unwrap();

        assert_eq!(selected, ["db", "api", "orphan"]);
    }

    #[test]
    fn copy_endpoint_rejects_non_project_service_names() {
        let config = ComposeConfig::from_yaml_str("services:\n  api:\n    image: api\n").unwrap();

        let error =
            resolve_copy_endpoint("project", &config, "other:/tmp/data".to_string()).unwrap_err();

        assert!(error
            .to_string()
            .contains("service 'other' is not defined in the Compose project"));
    }
}
