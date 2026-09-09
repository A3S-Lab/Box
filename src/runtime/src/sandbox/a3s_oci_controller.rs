//! Per-Sandbox A3S OCI Runtime owner startup.

use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use a3s_box_core::error::{BoxError, Result};
use a3s_oci_sdk::{
    ContainerId, ContainerTarget, CreateAttachments, CreateRequest, DeleteMode, DeleteRequest,
    DriverKind, IoMode, IsolationRequest, OciBundle, OciContainerState, OperationContext,
    OperationId, ProcessIo, RuntimeOperation, StartRequest,
};

use super::a3s_oci_client::A3sOciClient;
use super::a3s_oci_handler::{validate_record, A3sOciHandler, A3sOciHandlerSpec};
use super::controller::{
    bind_control_listener, create_private_dir, duplicate_for_inheritance, open_log, read_log_tail,
    reap_failed_log_worker, start_log_worker, write_json_atomic, SandboxLaunchSpec,
    EXEC_LISTENER_FD, INIT_LOG_FD, PTY_LISTENER_FD, START_TIMEOUT,
};
use super::runtime_record::SandboxRuntimeRecord;
use super::{linux_sandbox_delegated_cgroup_root, resolve_sandbox_oci_launcher, CertifiedA3sOci};

const START_FAILURE_LOG_LIMIT_BYTES: u64 = 4 * 1024;
/// Controller pinned to one verified runtime/agent artifact pair.
pub struct A3sOciController {
    runtime: CertifiedA3sOci,
}

impl A3sOciController {
    pub fn new(runtime: CertifiedA3sOci) -> Self {
        Self { runtime }
    }

    /// Refuse to overwrite any existing owner root for the same generation.
    pub fn require_absent(&self, runtime_root: &Path, container_id: &str) -> Result<()> {
        match std::fs::symlink_metadata(runtime_root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(BoxError::IoError(error)),
            Ok(_) => Err(BoxError::BoxBootError {
                message: format!(
                    "A3S OCI runtime root already exists for Sandbox {container_id}: {}",
                    runtime_root.display()
                ),
                hint: Some(
                    "Reconcile or remove the existing Sandbox generation before restarting it"
                        .to_string(),
                ),
            }),
        }
    }

    pub async fn start(&self, launch: SandboxLaunchSpec) -> Result<A3sOciHandler> {
        self.require_absent(&launch.runtime_root, &launch.container_id)?;
        let runtime_parent = launch.runtime_root.parent().ok_or_else(|| {
            BoxError::ConfigError("A3S OCI runtime root has no parent".to_string())
        })?;
        create_private_dir(runtime_parent)?;
        // Effective-root CI / setuid launchers create paths as euid 0. After
        // RootlessDevicePolicyBootstrap drops to the real UID, native-linux-service
        // requires the service root parent to be owned by that UID (mode 0700).
        chown_to_real_owner(runtime_parent)?;

        let exec_listener = bind_control_listener(&launch.exec_socket_path)?;
        let pty_listener = bind_control_listener(&launch.pty_socket_path)?;
        let stdout = open_log(&launch.stdout_path)?;
        let stderr = open_log(&launch.stderr_path)?;
        let init_log = open_log(&launch.init_log_path)?;

        let inherited_exec = duplicate_for_inheritance(exec_listener.as_raw_fd())?;
        let inherited_pty = duplicate_for_inheritance(pty_listener.as_raw_fd())?;
        let inherited_log = duplicate_for_inheritance(init_log.as_raw_fd())?;
        let exec_fd = inherited_exec.as_raw_fd();
        let pty_fd = inherited_pty.as_raw_fd();
        let log_fd = inherited_log.as_raw_fd();

        let delegated_cgroup_root = linux_sandbox_delegated_cgroup_root();
        let owner_cgroup = prepare_sandbox_delegation_child()?;
        let launcher = resolve_sandbox_oci_launcher(None)?;
        // Keep the launcher as the direct exec target. Intermediate bash/setpriv
        // wrappers break `--a3s-box-control-fds` (ExecListener must remain a Unix
        // stream on the fixed inherited descriptors). R17 stays on euid 0 so the
        // owner can bootstrap device policy without elevation.
        let mut command = Command::new(&launcher);
        command
            .arg("native-linux-service")
            .arg("--root")
            .arg(&launch.runtime_root)
            .arg("--agent")
            .arg(&self.runtime.agent_path)
            .arg("--delegated-cgroup-root")
            .arg(&delegated_cgroup_root)
            .arg("--container-id")
            .arg(&launch.container_id)
            .arg("--a3s-box-control-fds")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));

        // Sources are duplicated above 10, so installing the fixed Box roles
        // cannot clobber another source. `dup2` clears CLOEXEC on 3/4/5.
        // Migrate into a child below the empty delegated root so rootless open
        // accepts host-owned membership without moving the Sandbox CI harness
        // out of its probe cgroup.
        unsafe {
            command.pre_exec(move || {
                if let Some(ref cgroup) = owner_cgroup {
                    migrate_current_task_into_cgroup(cgroup)?;
                }
                for (source, destination) in [
                    (exec_fd, EXEC_LISTENER_FD),
                    (pty_fd, PTY_LISTENER_FD),
                    (log_fd, INIT_LOG_FD),
                ] {
                    if libc::dup2(source, destination) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }

        let mut owner = command.spawn().map_err(|error| BoxError::BoxBootError {
            message: format!("Failed to start A3S OCI runtime owner: {error}"),
            hint: None,
        })?;
        drop((inherited_exec, inherited_pty, inherited_log));
        drop((exec_listener, pty_listener, init_log));

        // Owner inherits effective root for device-policy bootstrap, then drops
        // to the real UID before publishing the SDK socket. Match that durable
        // identity here so SO_PEERCRED same-UID auth accepts the controller.
        drop_effective_root_to_real_owner()?;

        let owner_pid = owner.id();
        let owner_pid_start_time = crate::process::pid_start_time(owner_pid).ok_or_else(|| {
            let _ = owner.kill();
            let _ = owner.wait();
            BoxError::BoxBootError {
                message: "Failed to capture A3S OCI owner process identity".to_string(),
                hint: None,
            }
        })?;
        let runtime_socket = launch.runtime_root.join("runtime.sock");
        if let Err(error) = wait_for_private_socket(&mut owner, &runtime_socket).await {
            cleanup_failed_owner(&mut owner, owner_pid_start_time, None, None, &launch);
            return Err(with_start_diagnostics(error, &launch));
        }

        let mut client = None;
        let mut target = None;
        let lifecycle = async {
            let connected = A3sOciClient::connect(runtime_socket.clone()).await?;
            validate_required_operations(&connected.features()?)?;
            client = Some(connected);
            let lifecycle_client = client.as_ref().ok_or_else(|| {
                BoxError::StateError("A3S OCI client ownership was not retained".to_string())
            })?;

            let bundle = OciBundle::load(&launch.bundle_dir)
                .await
                .map_err(sdk_boot_error)?;
            let container_id =
                ContainerId::new(launch.container_id.clone()).map_err(sdk_boot_error)?;
            let attachments = CreateAttachments::from_bundle(
                &bundle,
                ProcessIo {
                    stdin: IoMode::Null,
                    stdout: IoMode::Inherit,
                    stderr: IoMode::Inherit,
                    terminal_size: None,
                },
            )
            .map_err(sdk_boot_error)?;
            let created = lifecycle_client.create(CreateRequest {
                context: operation_context(&launch.container_id, "create")?,
                id: container_id.clone(),
                bundle,
                isolation: IsolationRequest::SharedHostKernel,
                attachments,
            })?;
            let exact_target = ContainerTarget::exact(container_id, created.generation);
            validate_record(&created, &exact_target, None)?;
            if *created.state.status() != OciContainerState::Created {
                return Err(BoxError::StateError(
                    "A3S OCI create did not return the created state".to_string(),
                ));
            }
            let created_init_pid = created
                .state
                .pid()
                .and_then(|pid| u32::try_from(pid).ok())
                .ok_or_else(|| {
                    BoxError::StateError("A3S OCI create returned no valid init PID".to_string())
                })?;
            target = Some(exact_target.clone());

            let started = lifecycle_client.start(StartRequest {
                context: operation_context(&launch.container_id, "start")?,
                target: exact_target.clone(),
            })?;
            validate_record(&started, &exact_target, None)?;
            let started_status = *started.state.status();
            if started.driver != DriverKind::NativeLinux {
                return Err(BoxError::StateError(
                    "A3S OCI start did not return a native Linux container".to_string(),
                ));
            }
            let init_pid = started_generation_init_pid(
                created_init_pid,
                started_status,
                *started.state.pid(),
            )?;
            Ok((init_pid, started.generation))
        }
        .await;

        let (init_pid, generation) = match lifecycle {
            Ok(result) => result,
            Err(error) => {
                cleanup_failed_owner(
                    &mut owner,
                    owner_pid_start_time,
                    client.as_ref(),
                    target.as_ref(),
                    &launch,
                );
                return Err(with_start_diagnostics(error, &launch));
            }
        };
        let client = match client {
            Some(client) => client,
            None => {
                cleanup_failed_owner(&mut owner, owner_pid_start_time, None, None, &launch);
                return Err(BoxError::StateError(
                    "A3S OCI lifecycle completed without a retained SDK client".to_string(),
                ));
            }
        };
        let target = match target {
            Some(target) => target,
            None => {
                client.close();
                cleanup_failed_owner(&mut owner, owner_pid_start_time, None, None, &launch);
                return Err(BoxError::StateError(
                    "A3S OCI lifecycle completed without an exact target".to_string(),
                ));
            }
        };

        let mut log_worker = match start_log_worker(&launch, owner_pid, owner_pid_start_time) {
            Ok(worker) => worker,
            Err(error) => {
                cleanup_failed_owner(
                    &mut owner,
                    owner_pid_start_time,
                    Some(&client),
                    Some(&target),
                    &launch,
                );
                return Err(error);
            }
        };
        let log_worker_pid = log_worker.id();
        let log_worker_pid_start_time = match crate::process::pid_start_time(log_worker_pid) {
            Some(start_time) => start_time,
            None => {
                reap_failed_log_worker(&mut log_worker);
                cleanup_failed_owner(
                    &mut owner,
                    owner_pid_start_time,
                    Some(&client),
                    Some(&target),
                    &launch,
                );
                return Err(BoxError::BoxBootError {
                    message: "Failed to capture Sandbox log worker identity".to_string(),
                    hint: None,
                });
            }
        };

        let record = SandboxRuntimeRecord::a3s_oci(
            launch.container_id.clone(),
            self.runtime.runtime_path.clone(),
            self.runtime.runtime_sha256.clone(),
            self.runtime.agent_path.clone(),
            self.runtime.agent_sha256.clone(),
            launch.runtime_root.clone(),
            runtime_socket.clone(),
            launch.bundle_dir.clone(),
            init_pid,
            generation.0,
            owner_pid,
            owner_pid_start_time,
            log_worker_pid,
            log_worker_pid_start_time,
        );
        if let Err(error) = write_json_atomic(&launch.runtime_record, &record) {
            reap_failed_log_worker(&mut log_worker);
            cleanup_failed_owner(
                &mut owner,
                owner_pid_start_time,
                Some(&client),
                Some(&target),
                &launch,
            );
            return Err(error);
        }

        Ok(A3sOciHandler::from_child(
            A3sOciHandlerSpec {
                runtime_socket,
                runtime_root: launch.runtime_root,
                container_id: target.id,
                generation,
                init_pid,
                owner_pid,
                owner_pid_start_time,
                bundle_dir: launch.bundle_dir,
                runtime_record: launch.runtime_record,
            },
            client,
            owner,
            log_worker,
            log_worker_pid_start_time,
        ))
    }
}

async fn wait_for_private_socket(owner: &mut Child, socket_path: &Path) -> Result<()> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(status) = owner.try_wait().map_err(BoxError::IoError)? {
            return Err(BoxError::BoxBootError {
                message: format!("A3S OCI runtime owner exited before readiness: {status}"),
                hint: None,
            });
        }
        if private_socket_ready(socket_path)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "Timed out waiting for the A3S OCI runtime endpoint to become a private 0600 Unix socket: {}",
                    socket_path.display()
                ),
                hint: None,
            });
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn private_socket_ready(socket_path: &Path) -> Result<bool> {
    let expected_uid = expected_owner_uid();
    match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) if !metadata.file_type().is_socket() => {
            Err(private_socket_contract_error(socket_path))
        }
        // After rootless device-policy drop the socket is owned by the real UID
        // while effective-root harnesses still have euid 0. Accept the durable
        // owner identity, not geteuid().
        Ok(metadata) if metadata.uid() != expected_uid => {
            Err(private_socket_contract_error(socket_path))
        }
        // bind(2) publishes the same-owner socket before the Runtime owner can
        // chmod it. Keep polling without connecting until the exact private
        // mode is visible; a wrong type or owner still fails immediately.
        Ok(metadata) => Ok(metadata.permissions().mode() & 0o777 == 0o600),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

fn private_socket_contract_error(socket_path: &Path) -> BoxError {
    BoxError::BoxBootError {
        message: format!(
            "A3S OCI runtime endpoint failed its socket, owner, or mode contract: {}",
            socket_path.display()
        ),
        hint: None,
    }
}

fn validate_required_operations(info: &a3s_oci_sdk::RuntimeInfo) -> Result<()> {
    for operation in [
        RuntimeOperation::Create,
        RuntimeOperation::State,
        RuntimeOperation::Start,
        RuntimeOperation::Kill,
        RuntimeOperation::Delete,
        RuntimeOperation::Wait,
        RuntimeOperation::Pause,
        RuntimeOperation::Resume,
        RuntimeOperation::Update,
        RuntimeOperation::Stats,
    ] {
        if !info.operations.contains(&operation) {
            return Err(BoxError::BoxBootError {
                message: format!("A3S OCI runtime does not advertise {operation:?}"),
                hint: Some("Install the matching A3S OCI Runtime package".to_string()),
            });
        }
    }
    Ok(())
}

fn started_generation_init_pid(
    created_init_pid: u32,
    status: OciContainerState,
    started_init_pid: Option<i32>,
) -> Result<u32> {
    match status {
        OciContainerState::Running => {
            let started_init_pid = started_init_pid
                .and_then(|pid| u32::try_from(pid).ok())
                .ok_or_else(|| {
                    BoxError::StateError(
                        "A3S OCI running state returned no valid init PID".to_string(),
                    )
                })?;
            if started_init_pid != created_init_pid {
                return Err(BoxError::StateError(
                    "A3S OCI start changed the init PID".to_string(),
                ));
            }
            Ok(started_init_pid)
        }
        // OCI stopped state deliberately has no live PID. Retain the exact
        // PID allocated behind the create/start barrier so the durable Box
        // record still identifies this generation.
        OciContainerState::Stopped => Ok(created_init_pid),
        _ => Err(BoxError::StateError(
            "A3S OCI start did not return a running or stopped container".to_string(),
        )),
    }
}

fn operation_context(container_id: &str, operation: &str) -> Result<OperationContext> {
    OperationId::new(format!("{container_id}-{operation}"))
        .map(OperationContext::new)
        .map_err(sdk_boot_error)
}

fn cleanup_failed_owner(
    owner: &mut Child,
    owner_pid_start_time: u64,
    client: Option<&A3sOciClient>,
    target: Option<&ContainerTarget>,
    launch: &SandboxLaunchSpec,
) {
    if let (Some(client), Some(target)) = (client, target) {
        if let Ok(context) = operation_context(&launch.container_id, "failed-create-delete") {
            let _ = client.delete_if_present(DeleteRequest {
                context,
                target: target.clone(),
                mode: DeleteMode::Force,
            });
        }
        client.close();
    }
    let pid = owner.id();
    if super::a3s_oci_owner::stop(pid, owner_pid_start_time).is_err() {
        let _ = owner.kill();
    }
    let _ = owner.wait();
    let _ = std::fs::remove_file(&launch.runtime_record);
    let _ = std::fs::remove_dir_all(&launch.runtime_root);
}

fn with_start_diagnostics(error: BoxError, launch: &SandboxLaunchSpec) -> BoxError {
    let diagnostics = [
        ("runtime/container stderr", &launch.stderr_path),
        ("guest-init log", &launch.init_log_path),
    ]
    .into_iter()
    .filter_map(|(label, path)| {
        read_log_tail(path, START_FAILURE_LOG_LIMIT_BYTES)
            .map(|excerpt| format!("{label}: {excerpt}"))
    })
    .collect::<Vec<_>>();
    if diagnostics.is_empty() {
        error
    } else {
        BoxError::BoxBootError {
            message: format!("{error} ({})", diagnostics.join("; ")),
            hint: None,
        }
    }
}

fn sdk_boot_error(error: a3s_oci_sdk::Error) -> BoxError {
    BoxError::BoxBootError {
        message: format!("A3S OCI SDK rejected Sandbox launch: {error}"),
        hint: None,
    }
}

fn prepare_sandbox_delegation_child() -> Result<Option<PathBuf>> {
    prepare_sandbox_delegation_child_in(PathBuf::from(linux_sandbox_delegated_cgroup_root()))
}

fn prepare_sandbox_delegation_child_in(root: PathBuf) -> Result<Option<PathBuf>> {
    if !root.is_dir() {
        return Ok(None);
    }
    let child = root.join(format!("box-sandbox-owner-{}", std::process::id()));
    std::fs::create_dir_all(&child).map_err(|error| BoxError::BoxBootError {
        message: format!(
            "failed to create Sandbox OCI owner cgroup {}: {error}",
            child.display()
        ),
        hint: None,
    })?;
    chown_to_real_owner(&child)?;
    for name in [
        "cgroup.procs",
        "cgroup.subtree_control",
        "cgroup.controllers",
    ] {
        let control = child.join(name);
        if control.exists() {
            chown_to_real_owner(&control)?;
        }
    }
    Ok(Some(child))
}

fn migrate_current_task_into_cgroup(cgroup: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let procs = cgroup.join("cgroup.procs");
    let path = std::ffi::CString::new(procs.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cgroup.procs path contains an interior NUL",
        )
    })?;
    // SAFETY: open/write/close on cgroup.procs between fork and exec; path is
    // NUL-terminated and owned for the duration of the calls.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let written = unsafe { libc::write(fd, b"0".as_ptr().cast(), 1) };
    let close_rc = unsafe { libc::close(fd) };
    if written != 1 {
        return Err(if written < 0 {
            std::io::Error::last_os_error()
        } else {
            std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "short write migrating into Sandbox owner cgroup",
            )
        });
    }
    if close_rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn chown_to_real_owner(path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let (uid, gid) = expected_owner_ids();
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        BoxError::ConfigError(format!(
            "Sandbox OCI path contains an interior NUL: {}",
            path.display()
        ))
    })?;
    // SAFETY: chown takes a NUL-terminated path owned for the duration of the call.
    let rc = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
    if rc != 0 {
        return Err(BoxError::BoxBootError {
            message: format!(
                "failed to assign Sandbox OCI path {} to UID {uid}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ),
            hint: None,
        });
    }
    Ok(())
}

/// Durable filesystem identity for Sandbox OCI owners.
///
/// Under setpriv / setuid launchers (euid 0, non-root ruid), ownership follows
/// the real UID so paths remain usable after rootless device-policy drop.
fn expected_owner_ids() -> (u32, u32) {
    // SAFETY: credential queries have no pointer arguments or failure results.
    let (ruid, rgid, euid, egid) = unsafe {
        (
            libc::getuid(),
            libc::getgid(),
            libc::geteuid(),
            libc::getegid(),
        )
    };
    if euid == 0 && ruid != 0 {
        (ruid, rgid)
    } else {
        (euid, egid)
    }
}

fn expected_owner_uid() -> u32 {
    expected_owner_ids().0
}

/// Permanently match the Sandbox controller to the post-bootstrap owner UID.
///
/// Effective-root CI / setuid launchers keep euid 0 through rootfs prep and
/// owner spawn so `native-linux-service` can install the device-policy helper.
/// After spawn, the owner drops to the real UID and rejects other peer UIDs on
/// the SDK socket. Dropping here is process-wide and intentional for that
/// connection lifetime.
fn drop_effective_root_to_real_owner() -> Result<()> {
    let (ruid, rgid, euid, egid) = unsafe {
        (
            libc::getuid(),
            libc::getgid(),
            libc::geteuid(),
            libc::getegid(),
        )
    };
    if euid != 0 || ruid == 0 {
        return Ok(());
    }
    if egid != rgid {
        // SAFETY: setegid takes a gid_t; failure is reported via errno.
        if unsafe { libc::setegid(rgid) } != 0 {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "failed to drop Sandbox controller egid to {rgid}: {}",
                    std::io::Error::last_os_error()
                ),
                hint: None,
            });
        }
    }
    // SAFETY: seteuid takes a uid_t; failure is reported via errno.
    if unsafe { libc::seteuid(ruid) } != 0 {
        return Err(BoxError::BoxBootError {
            message: format!(
                "failed to drop Sandbox controller euid to {ruid}: {}",
                std::io::Error::last_os_error()
            ),
            hint: None,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_socket_waits_for_the_owner_to_finish_tightening_its_mode() {
        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("runtime.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();

        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!private_socket_ready(&socket_path).unwrap());

        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(private_socket_ready(&socket_path).unwrap());
    }

    #[test]
    fn private_socket_rejects_a_non_socket_endpoint_without_waiting() {
        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("runtime.sock");
        std::fs::write(&socket_path, b"not a socket").unwrap();

        let error = private_socket_ready(&socket_path).unwrap_err();

        assert!(error
            .to_string()
            .contains("failed its socket, owner, or mode contract"));
    }

    #[test]
    fn stopped_start_retains_the_created_generation_pid() {
        let init_pid =
            started_generation_init_pid(4_242, OciContainerState::Stopped, None).unwrap();

        assert_eq!(init_pid, 4_242);
    }

    #[test]
    fn running_start_requires_the_same_generation_pid() {
        let error = started_generation_init_pid(4_242, OciContainerState::Running, Some(4_243))
            .unwrap_err();

        assert!(error.to_string().contains("changed the init PID"));
    }

    #[test]
    fn start_rejects_non_terminal_contract_states() {
        let error = started_generation_init_pid(4_242, OciContainerState::Created, Some(4_242))
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("did not return a running or stopped container"));
    }

    #[test]
    fn prepares_a_child_below_the_delegated_cgroup_root() {
        let root = tempfile::tempdir().unwrap();
        let child = prepare_sandbox_delegation_child_in(root.path().to_path_buf())
            .unwrap()
            .expect("delegated root exists");

        assert!(child.starts_with(root.path()));
        assert!(child.is_dir());
        assert!(child
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("box-sandbox-owner-"));
    }

    #[test]
    fn skips_delegation_child_when_the_root_is_absent() {
        let missing = tempfile::tempdir().unwrap().path().join("missing");
        assert!(prepare_sandbox_delegation_child_in(missing)
            .unwrap()
            .is_none());
    }

    #[test]
    fn assigns_private_dirs_to_the_real_owner_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime-parent");
        create_private_dir(&path).unwrap();
        chown_to_real_owner(&path).unwrap();

        let metadata = std::fs::metadata(&path).unwrap();
        let expected = expected_owner_uid();
        assert_eq!(metadata.uid(), expected);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    }
}
