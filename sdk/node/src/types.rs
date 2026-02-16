use napi_derive::napi;

/// Execution metrics.
#[napi(object)]
#[derive(Clone)]
pub struct JsExecMetrics {
    pub duration_ms: u32,
    pub stdout_bytes: u32,
    pub stderr_bytes: u32,
}

/// Result of executing a command.
#[napi(object)]
#[derive(Clone)]
pub struct JsExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub metrics: JsExecMetrics,
}

/// Sandbox configuration options.
#[napi(object)]
#[derive(Clone)]
pub struct JsSandboxOptions {
    pub image: Option<String>,
    pub cpus: Option<u32>,
    pub memory_mb: Option<u32>,
    pub env: Option<std::collections::HashMap<String, String>>,
    pub workdir: Option<String>,
    pub mounts: Option<Vec<JsMountSpec>>,
    pub network: Option<bool>,
    pub tee: Option<bool>,
    pub name: Option<String>,
    pub port_forwards: Option<Vec<JsPortForward>>,
    pub workspace: Option<JsWorkspaceConfig>,
}

/// Host-to-guest mount specification.
#[napi(object)]
#[derive(Clone)]
pub struct JsMountSpec {
    pub host_path: String,
    pub guest_path: String,
    pub readonly: Option<bool>,
}

/// Port forwarding rule.
#[napi(object)]
#[derive(Clone)]
pub struct JsPortForward {
    pub guest_port: u16,
    pub host_port: Option<u16>,
    pub protocol: Option<String>,
}

/// Persistent workspace configuration.
#[napi(object)]
#[derive(Clone)]
pub struct JsWorkspaceConfig {
    pub name: String,
    pub guest_path: Option<String>,
}

impl From<&JsSandboxOptions> for a3s_box_sdk::SandboxOptions {
    fn from(opts: &JsSandboxOptions) -> Self {
        let mut o = Self::default();
        if let Some(ref image) = opts.image {
            o.image = image.clone();
        }
        if let Some(cpus) = opts.cpus {
            o.cpus = cpus;
        }
        if let Some(mem) = opts.memory_mb {
            o.memory_mb = mem;
        }
        if let Some(ref env) = opts.env {
            o.env = env.clone();
        }
        o.workdir = opts.workdir.clone();
        if let Some(ref mounts) = opts.mounts {
            o.mounts = mounts
                .iter()
                .map(|m| a3s_box_sdk::MountSpec {
                    host_path: m.host_path.clone(),
                    guest_path: m.guest_path.clone(),
                    readonly: m.readonly.unwrap_or(false),
                })
                .collect();
        }
        if let Some(network) = opts.network {
            o.network = network;
        }
        if let Some(tee) = opts.tee {
            o.tee = tee;
        }
        o.name = opts.name.clone();
        if let Some(ref pfs) = opts.port_forwards {
            o.port_forwards = pfs
                .iter()
                .map(|p| a3s_box_sdk::PortForward {
                    guest_port: p.guest_port,
                    host_port: p.host_port.unwrap_or(0),
                    protocol: p.protocol.clone().unwrap_or_else(|| "tcp".into()),
                })
                .collect();
        }
        if let Some(ref ws) = opts.workspace {
            o.workspace = Some(a3s_box_sdk::WorkspaceConfig {
                name: ws.name.clone(),
                guest_path: ws.guest_path.clone().unwrap_or_else(|| "/workspace".into()),
            });
        }
        o
    }
}
