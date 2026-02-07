//! CRI RuntimeService implementation.
//!
//! Maps CRI pod/container lifecycle to A3S Box VmManager instances.
//! - Pod Sandbox → Box instance (one microVM per pod)
//! - Container → Session within Box

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tonic::{Request, Response, Status};

use a3s_box_core::event::EventEmitter;
use a3s_box_runtime::oci::{ImagePuller, ImageStore, RegistryAuth};
use a3s_box_runtime::vm::VmManager;

use crate::config_mapper::pod_sandbox_config_to_box_config;
use crate::container::{Container, ContainerState, ContainerStore};
use crate::cri_api::runtime_service_server::RuntimeService;
use crate::cri_api::*;
use crate::error::box_error_to_status;
use crate::sandbox::{PodSandbox, SandboxState, SandboxStore};

/// A3S Box implementation of the CRI RuntimeService.
pub struct BoxRuntimeService {
    sandbox_store: Arc<SandboxStore>,
    container_store: Arc<ContainerStore>,
    image_store: Arc<ImageStore>,
    image_puller: Arc<ImagePuller>,
    /// Maps sandbox_id → VmManager for running VMs.
    vm_managers: Arc<RwLock<HashMap<String, VmManager>>>,
}

impl BoxRuntimeService {
    /// Create a new BoxRuntimeService.
    pub fn new(image_store: Arc<ImageStore>, auth: RegistryAuth) -> Self {
        let image_puller = Arc::new(ImagePuller::new(image_store.clone(), auth));
        Self {
            sandbox_store: Arc::new(SandboxStore::new()),
            container_store: Arc::new(ContainerStore::new()),
            image_store,
            image_puller,
            vm_managers: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[tonic::async_trait]
impl RuntimeService for BoxRuntimeService {
    // ── Version ──────────────────────────────────────────────────────

    async fn version(
        &self,
        request: Request<VersionRequest>,
    ) -> Result<Response<VersionResponse>, Status> {
        let _req = request.into_inner();
        Ok(Response::new(VersionResponse {
            version: "0.1.0".to_string(),
            runtime_name: "a3s-box".to_string(),
            runtime_version: a3s_box_runtime::VERSION.to_string(),
            runtime_api_version: "v1".to_string(),
        }))
    }

    // ── Pod Sandbox ──────────────────────────────────────────────────

    async fn run_pod_sandbox(
        &self,
        request: Request<RunPodSandboxRequest>,
    ) -> Result<Response<RunPodSandboxResponse>, Status> {
        let req = request.into_inner();
        let config = req
            .config
            .ok_or_else(|| Status::invalid_argument("sandbox config required"))?;

        let metadata = config
            .metadata
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("sandbox metadata required"))?;

        tracing::info!(
            name = %metadata.name,
            namespace = %metadata.namespace,
            "CRI RunPodSandbox"
        );

        // Convert CRI config to BoxConfig
        let box_config =
            pod_sandbox_config_to_box_config(&config).map_err(box_error_to_status)?;

        // Create VmManager
        let event_emitter = EventEmitter::new(256);
        let mut vm = VmManager::new(box_config, event_emitter);
        let sandbox_id = vm.box_id().to_string();

        // Boot the VM
        vm.boot().await.map_err(box_error_to_status)?;

        // Store sandbox state
        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let sandbox = PodSandbox {
            id: sandbox_id.clone(),
            name: metadata.name.clone(),
            namespace: metadata.namespace.clone(),
            uid: metadata.uid.clone(),
            state: SandboxState::Ready,
            created_at: now_ns,
            labels: config.labels.clone(),
            annotations: config.annotations.clone(),
            log_directory: config.log_directory.clone(),
            runtime_handler: req.runtime_handler,
        };

        self.sandbox_store.add(sandbox).await;
        self.vm_managers
            .write()
            .await
            .insert(sandbox_id.clone(), vm);

        Ok(Response::new(RunPodSandboxResponse {
            pod_sandbox_id: sandbox_id,
        }))
    }

    async fn stop_pod_sandbox(
        &self,
        request: Request<StopPodSandboxRequest>,
    ) -> Result<Response<StopPodSandboxResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = &req.pod_sandbox_id;

        tracing::info!(sandbox_id = %sandbox_id, "CRI StopPodSandbox");

        // Stop all containers in this sandbox
        let containers = self.container_store.list(Some(sandbox_id), None).await;
        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        for c in &containers {
            if c.state != ContainerState::Exited {
                self.container_store
                    .mark_exited(&c.id, now_ns, 137)
                    .await;
            }
        }

        // Destroy the VM
        if let Some(mut vm) = self.vm_managers.write().await.remove(sandbox_id) {
            vm.destroy().await.map_err(box_error_to_status)?;
        }

        self.sandbox_store
            .update_state(sandbox_id, SandboxState::NotReady)
            .await;

        Ok(Response::new(StopPodSandboxResponse {}))
    }

    async fn remove_pod_sandbox(
        &self,
        request: Request<RemovePodSandboxRequest>,
    ) -> Result<Response<RemovePodSandboxResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = &req.pod_sandbox_id;

        tracing::info!(sandbox_id = %sandbox_id, "CRI RemovePodSandbox");

        // Ensure VM is stopped
        if let Some(mut vm) = self.vm_managers.write().await.remove(sandbox_id) {
            let _ = vm.destroy().await;
        }

        // Remove all containers
        self.container_store.remove_by_sandbox(sandbox_id).await;

        // Remove sandbox
        self.sandbox_store.remove(sandbox_id).await;

        Ok(Response::new(RemovePodSandboxResponse {}))
    }

    async fn pod_sandbox_status(
        &self,
        request: Request<PodSandboxStatusRequest>,
    ) -> Result<Response<PodSandboxStatusResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = &req.pod_sandbox_id;

        let sandbox = self
            .sandbox_store
            .get(sandbox_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Sandbox not found: {}", sandbox_id)))?;

        let state = match sandbox.state {
            SandboxState::Ready => PodSandboxState::SandboxReady,
            SandboxState::NotReady | SandboxState::Removed => PodSandboxState::SandboxNotready,
        };

        let status = PodSandboxStatus {
            id: sandbox.id.clone(),
            metadata: Some(PodSandboxMetadata {
                name: sandbox.name.clone(),
                uid: sandbox.uid.clone(),
                namespace: sandbox.namespace.clone(),
                attempt: 0,
            }),
            state: state.into(),
            created_at: sandbox.created_at,
            network: Some(PodSandboxNetworkStatus {
                ip: String::new(),
                additional_ips: vec![],
            }),
            linux: None,
            labels: sandbox.labels.clone(),
            annotations: sandbox.annotations.clone(),
            runtime_handler: sandbox.runtime_handler.clone(),
        };

        Ok(Response::new(PodSandboxStatusResponse {
            status: Some(status),
            info: Default::default(),
        }))
    }

    async fn list_pod_sandbox(
        &self,
        request: Request<ListPodSandboxRequest>,
    ) -> Result<Response<ListPodSandboxResponse>, Status> {
        let req = request.into_inner();

        let label_filter = req
            .filter
            .as_ref()
            .map(|f| &f.label_selector)
            .filter(|m| !m.is_empty());

        let sandboxes = self.sandbox_store.list(label_filter).await;

        let items: Vec<crate::cri_api::PodSandbox> = sandboxes
            .into_iter()
            .filter(|sb| {
                if let Some(ref filter) = req.filter {
                    // Filter by ID
                    if !filter.id.is_empty() && sb.id != filter.id {
                        return false;
                    }
                    // Filter by state
                    let sb_state = match sb.state {
                        SandboxState::Ready => PodSandboxState::SandboxReady as i32,
                        _ => PodSandboxState::SandboxNotready as i32,
                    };
                    if filter.state != 0 && filter.state != sb_state {
                        return false;
                    }
                }
                true
            })
            .map(|sb| {
                let state = match sb.state {
                    SandboxState::Ready => PodSandboxState::SandboxReady,
                    _ => PodSandboxState::SandboxNotready,
                };
                crate::cri_api::PodSandbox {
                    id: sb.id,
                    metadata: Some(PodSandboxMetadata {
                        name: sb.name,
                        uid: sb.uid,
                        namespace: sb.namespace,
                        attempt: 0,
                    }),
                    state: state.into(),
                    created_at: sb.created_at,
                    labels: sb.labels,
                    annotations: sb.annotations,
                    runtime_handler: sb.runtime_handler,
                }
            })
            .collect();

        Ok(Response::new(ListPodSandboxResponse { items }))
    }

    // ── Container ────────────────────────────────────────────────────

    async fn create_container(
        &self,
        request: Request<CreateContainerRequest>,
    ) -> Result<Response<CreateContainerResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = &req.pod_sandbox_id;

        // Verify sandbox exists
        self.sandbox_store
            .get(sandbox_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Sandbox not found: {}", sandbox_id)))?;

        let config = req
            .config
            .ok_or_else(|| Status::invalid_argument("container config required"))?;

        let metadata = config
            .metadata
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("container metadata required"))?;

        let image_ref = config
            .image
            .as_ref()
            .map(|i| i.image.clone())
            .unwrap_or_default();

        tracing::info!(
            sandbox_id = %sandbox_id,
            name = %metadata.name,
            image = %image_ref,
            "CRI CreateContainer"
        );

        let container_id = uuid::Uuid::new_v4().to_string();
        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);

        let container = Container {
            id: container_id.clone(),
            sandbox_id: sandbox_id.to_string(),
            name: metadata.name.clone(),
            image_ref,
            state: ContainerState::Created,
            created_at: now_ns,
            started_at: 0,
            finished_at: 0,
            exit_code: 0,
            labels: config.labels.clone(),
            annotations: config.annotations.clone(),
            log_path: config.log_path,
        };

        self.container_store.add(container).await;

        Ok(Response::new(CreateContainerResponse { container_id }))
    }

    async fn start_container(
        &self,
        request: Request<StartContainerRequest>,
    ) -> Result<Response<StartContainerResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        let container = self
            .container_store
            .get(container_id)
            .await
            .ok_or_else(|| {
                Status::not_found(format!("Container not found: {}", container_id))
            })?;

        tracing::info!(
            container_id = %container_id,
            sandbox_id = %container.sandbox_id,
            "CRI StartContainer"
        );

        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        self.container_store
            .mark_started(container_id, now_ns)
            .await;

        Ok(Response::new(StartContainerResponse {}))
    }

    async fn stop_container(
        &self,
        request: Request<StopContainerRequest>,
    ) -> Result<Response<StopContainerResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        tracing::info!(container_id = %container_id, "CRI StopContainer");

        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        self.container_store
            .mark_exited(container_id, now_ns, 0)
            .await;

        Ok(Response::new(StopContainerResponse {}))
    }

    async fn remove_container(
        &self,
        request: Request<RemoveContainerRequest>,
    ) -> Result<Response<RemoveContainerResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        tracing::info!(container_id = %container_id, "CRI RemoveContainer");

        self.container_store.remove(container_id).await;

        Ok(Response::new(RemoveContainerResponse {}))
    }

    async fn container_status(
        &self,
        request: Request<ContainerStatusRequest>,
    ) -> Result<Response<ContainerStatusResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        let container = self
            .container_store
            .get(container_id)
            .await
            .ok_or_else(|| {
                Status::not_found(format!("Container not found: {}", container_id))
            })?;

        let state = match container.state {
            ContainerState::Created => crate::cri_api::ContainerState::ContainerCreated,
            ContainerState::Running => crate::cri_api::ContainerState::ContainerRunning,
            ContainerState::Exited => crate::cri_api::ContainerState::ContainerExited,
        };

        let status = ContainerStatus {
            id: container.id.clone(),
            metadata: Some(ContainerMetadata {
                name: container.name.clone(),
                attempt: 0,
            }),
            state: state.into(),
            created_at: container.created_at,
            started_at: container.started_at,
            finished_at: container.finished_at,
            exit_code: container.exit_code,
            image: Some(ImageSpec {
                image: container.image_ref.clone(),
                annotations: Default::default(),
            }),
            image_ref: container.image_ref.clone(),
            reason: String::new(),
            message: String::new(),
            labels: container.labels.clone(),
            annotations: container.annotations.clone(),
            mounts: vec![],
            log_path: container.log_path.clone(),
        };

        Ok(Response::new(ContainerStatusResponse {
            status: Some(status),
            info: Default::default(),
        }))
    }

    async fn list_containers(
        &self,
        request: Request<ListContainersRequest>,
    ) -> Result<Response<ListContainersResponse>, Status> {
        let req = request.into_inner();

        let sandbox_filter = req
            .filter
            .as_ref()
            .map(|f| f.pod_sandbox_id.as_str())
            .filter(|s| !s.is_empty());

        let label_filter = req
            .filter
            .as_ref()
            .map(|f| &f.label_selector)
            .filter(|m| !m.is_empty());

        let containers = self
            .container_store
            .list(sandbox_filter, label_filter)
            .await;

        let items: Vec<crate::cri_api::Container> = containers
            .into_iter()
            .filter(|c| {
                if let Some(ref filter) = req.filter {
                    if !filter.id.is_empty() && c.id != filter.id {
                        return false;
                    }
                    if let Some(ref state_val) = filter.state {
                        let c_state = match c.state {
                            ContainerState::Created => {
                                crate::cri_api::ContainerState::ContainerCreated as i32
                            }
                            ContainerState::Running => {
                                crate::cri_api::ContainerState::ContainerRunning as i32
                            }
                            ContainerState::Exited => {
                                crate::cri_api::ContainerState::ContainerExited as i32
                            }
                        };
                        if state_val.state != c_state {
                            return false;
                        }
                    }
                }
                true
            })
            .map(|c| {
                let state = match c.state {
                    ContainerState::Created => {
                        crate::cri_api::ContainerState::ContainerCreated
                    }
                    ContainerState::Running => {
                        crate::cri_api::ContainerState::ContainerRunning
                    }
                    ContainerState::Exited => {
                        crate::cri_api::ContainerState::ContainerExited
                    }
                };
                crate::cri_api::Container {
                    id: c.id,
                    pod_sandbox_id: c.sandbox_id,
                    metadata: Some(ContainerMetadata {
                        name: c.name,
                        attempt: 0,
                    }),
                    image: Some(ImageSpec {
                        image: c.image_ref.clone(),
                        annotations: Default::default(),
                    }),
                    image_ref: c.image_ref,
                    state: state.into(),
                    created_at: c.created_at,
                    labels: c.labels,
                    annotations: c.annotations,
                }
            })
            .collect();

        Ok(Response::new(ListContainersResponse { containers: items }))
    }

    // ── Status ───────────────────────────────────────────────────────

    async fn status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let conditions = vec![
            RuntimeCondition {
                r#type: "RuntimeReady".to_string(),
                status: true,
                reason: String::new(),
                message: String::new(),
            },
            RuntimeCondition {
                r#type: "NetworkReady".to_string(),
                status: true,
                reason: String::new(),
                message: String::new(),
            },
        ];

        Ok(Response::new(StatusResponse {
            status: Some(RuntimeStatus { conditions }),
            info: Default::default(),
        }))
    }

    async fn update_runtime_config(
        &self,
        _request: Request<UpdateRuntimeConfigRequest>,
    ) -> Result<Response<UpdateRuntimeConfigResponse>, Status> {
        // Accept but ignore runtime config updates for now
        Ok(Response::new(UpdateRuntimeConfigResponse {}))
    }

    // ── Exec / Attach / PortForward (P2 - stubs) ────────────────────

    async fn exec_sync(
        &self,
        request: Request<ExecSyncRequest>,
    ) -> Result<Response<ExecSyncResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        tracing::info!(container_id = %container_id, "CRI ExecSync");

        // Look up the container to find its sandbox
        let container = self
            .container_store
            .get(container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;

        // Get the VmManager for this sandbox
        let vm_managers = self.vm_managers.read().await;
        let vm = vm_managers
            .get(&container.sandbox_id)
            .ok_or_else(|| {
                Status::not_found(format!("Sandbox not found: {}", container.sandbox_id))
            })?;

        // Get the agent client from the VM
        let client = vm.agent_client().ok_or_else(|| {
            Status::unavailable("Agent not connected for this sandbox")
        })?;

        // Join the command into a single string to send as a prompt
        let cmd = req.cmd.join(" ");

        // Execute via the agent's Generate RPC
        let result = client
            .generate(a3s_box_runtime::grpc::GenerateRequest {
                session_id: String::new(),
                prompt: cmd,
            })
            .await
            .map_err(box_error_to_status)?;

        Ok(Response::new(ExecSyncResponse {
            stdout: result.text.into_bytes(),
            stderr: vec![],
            exit_code: 0,
        }))
    }

    async fn exec(
        &self,
        _request: Request<ExecRequest>,
    ) -> Result<Response<ExecResponse>, Status> {
        Err(Status::unimplemented("Exec not yet implemented"))
    }

    async fn attach(
        &self,
        _request: Request<AttachRequest>,
    ) -> Result<Response<AttachResponse>, Status> {
        Err(Status::unimplemented("Attach not yet implemented"))
    }

    async fn port_forward(
        &self,
        _request: Request<PortForwardRequest>,
    ) -> Result<Response<PortForwardResponse>, Status> {
        Err(Status::unimplemented("PortForward not yet implemented"))
    }

    async fn update_container_resources(
        &self,
        _request: Request<UpdateContainerResourcesRequest>,
    ) -> Result<Response<UpdateContainerResourcesResponse>, Status> {
        Err(Status::unimplemented(
            "UpdateContainerResources not yet implemented",
        ))
    }

    async fn reopen_container_log(
        &self,
        _request: Request<ReopenContainerLogRequest>,
    ) -> Result<Response<ReopenContainerLogResponse>, Status> {
        Err(Status::unimplemented(
            "ReopenContainerLog not yet implemented",
        ))
    }
}
