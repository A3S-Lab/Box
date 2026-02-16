use pyo3::prelude::*;

use a3s_box_core::exec::ExecMetrics;

/// Execution metrics.
#[pyclass]
#[derive(Clone)]
pub struct PyExecMetrics {
    #[pyo3(get)]
    pub duration_ms: u64,
    #[pyo3(get)]
    pub stdout_bytes: u64,
    #[pyo3(get)]
    pub stderr_bytes: u64,
}

#[pymethods]
impl PyExecMetrics {
    fn __repr__(&self) -> String {
        format!(
            "ExecMetrics(duration_ms={}, stdout_bytes={}, stderr_bytes={})",
            self.duration_ms, self.stdout_bytes, self.stderr_bytes
        )
    }
}

impl From<ExecMetrics> for PyExecMetrics {
    fn from(m: ExecMetrics) -> Self {
        Self {
            duration_ms: m.duration_ms,
            stdout_bytes: m.stdout_bytes,
            stderr_bytes: m.stderr_bytes,
        }
    }
}

/// Result of executing a command.
#[pyclass]
#[derive(Clone)]
pub struct PyExecResult {
    #[pyo3(get)]
    pub stdout: String,
    #[pyo3(get)]
    pub stderr: String,
    #[pyo3(get)]
    pub exit_code: i32,
    #[pyo3(get)]
    pub metrics: PyExecMetrics,
}

#[pymethods]
impl PyExecResult {
    fn __repr__(&self) -> String {
        format!(
            "ExecResult(exit_code={}, stdout_len={}, stderr_len={})",
            self.exit_code,
            self.stdout.len(),
            self.stderr.len()
        )
    }
}

/// Sandbox configuration options.
#[pyclass]
#[derive(Clone)]
pub struct PySandboxOptions {
    #[pyo3(get, set)]
    pub image: String,
    #[pyo3(get, set)]
    pub cpus: u32,
    #[pyo3(get, set)]
    pub memory_mb: u32,
    #[pyo3(get, set)]
    pub env: std::collections::HashMap<String, String>,
    #[pyo3(get, set)]
    pub workdir: Option<String>,
    #[pyo3(get, set)]
    pub mounts: Vec<PyMountSpec>,
    #[pyo3(get, set)]
    pub network: bool,
    #[pyo3(get, set)]
    pub tee: bool,
    #[pyo3(get, set)]
    pub name: Option<String>,
    #[pyo3(get, set)]
    pub port_forwards: Vec<PyPortForward>,
    #[pyo3(get, set)]
    pub workspace: Option<PyWorkspaceConfig>,
}

#[pymethods]
impl PySandboxOptions {
    #[new]
    #[pyo3(signature = (
        image = "alpine:latest".to_string(),
        cpus = 1,
        memory_mb = 256,
        env = None,
        workdir = None,
        mounts = None,
        network = true,
        tee = false,
        name = None,
        port_forwards = None,
        workspace = None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        image: String,
        cpus: u32,
        memory_mb: u32,
        env: Option<std::collections::HashMap<String, String>>,
        workdir: Option<String>,
        mounts: Option<Vec<PyMountSpec>>,
        network: bool,
        tee: bool,
        name: Option<String>,
        port_forwards: Option<Vec<PyPortForward>>,
        workspace: Option<PyWorkspaceConfig>,
    ) -> Self {
        Self {
            image,
            cpus,
            memory_mb,
            env: env.unwrap_or_default(),
            workdir,
            mounts: mounts.unwrap_or_default(),
            network,
            tee,
            name,
            port_forwards: port_forwards.unwrap_or_default(),
            workspace,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "SandboxOptions(image='{}', cpus={}, memory_mb={})",
            self.image, self.cpus, self.memory_mb
        )
    }
}

impl From<&PySandboxOptions> for a3s_box_sdk::SandboxOptions {
    fn from(opts: &PySandboxOptions) -> Self {
        Self {
            image: opts.image.clone(),
            cpus: opts.cpus,
            memory_mb: opts.memory_mb,
            env: opts.env.clone(),
            workdir: opts.workdir.clone(),
            mounts: opts.mounts.iter().map(|m| m.into()).collect(),
            network: opts.network,
            tee: opts.tee,
            name: opts.name.clone(),
            port_forwards: opts.port_forwards.iter().map(|p| p.into()).collect(),
            workspace: opts.workspace.as_ref().map(|w| w.into()),
        }
    }
}

/// Host-to-guest mount specification.
#[pyclass]
#[derive(Clone)]
pub struct PyMountSpec {
    #[pyo3(get, set)]
    pub host_path: String,
    #[pyo3(get, set)]
    pub guest_path: String,
    #[pyo3(get, set)]
    pub readonly: bool,
}

#[pymethods]
impl PyMountSpec {
    #[new]
    #[pyo3(signature = (host_path, guest_path, readonly = false))]
    fn new(host_path: String, guest_path: String, readonly: bool) -> Self {
        Self { host_path, guest_path, readonly }
    }

    fn __repr__(&self) -> String {
        format!("MountSpec('{}' -> '{}', ro={})", self.host_path, self.guest_path, self.readonly)
    }
}

impl From<&PyMountSpec> for a3s_box_sdk::MountSpec {
    fn from(m: &PyMountSpec) -> Self {
        Self {
            host_path: m.host_path.clone(),
            guest_path: m.guest_path.clone(),
            readonly: m.readonly,
        }
    }
}

/// Port forwarding rule.
#[pyclass]
#[derive(Clone)]
pub struct PyPortForward {
    #[pyo3(get, set)]
    pub guest_port: u16,
    #[pyo3(get, set)]
    pub host_port: u16,
    #[pyo3(get, set)]
    pub protocol: String,
}

#[pymethods]
impl PyPortForward {
    #[new]
    #[pyo3(signature = (guest_port, host_port = 0, protocol = "tcp".to_string()))]
    fn new(guest_port: u16, host_port: u16, protocol: String) -> Self {
        Self { guest_port, host_port, protocol }
    }

    fn __repr__(&self) -> String {
        format!("PortForward({}:{} {})", self.host_port, self.guest_port, self.protocol)
    }
}

impl From<&PyPortForward> for a3s_box_sdk::PortForward {
    fn from(p: &PyPortForward) -> Self {
        Self {
            guest_port: p.guest_port,
            host_port: p.host_port,
            protocol: p.protocol.clone(),
        }
    }
}

/// Persistent workspace configuration.
#[pyclass]
#[derive(Clone)]
pub struct PyWorkspaceConfig {
    #[pyo3(get, set)]
    pub name: String,
    #[pyo3(get, set)]
    pub guest_path: String,
}

#[pymethods]
impl PyWorkspaceConfig {
    #[new]
    #[pyo3(signature = (name, guest_path = "/workspace".to_string()))]
    fn new(name: String, guest_path: String) -> Self {
        Self { name, guest_path }
    }

    fn __repr__(&self) -> String {
        format!("WorkspaceConfig(name='{}', guest_path='{}')", self.name, self.guest_path)
    }
}

impl From<&PyWorkspaceConfig> for a3s_box_sdk::WorkspaceConfig {
    fn from(w: &PyWorkspaceConfig) -> Self {
        Self {
            name: w.name.clone(),
            guest_path: w.guest_path.clone(),
        }
    }
}
