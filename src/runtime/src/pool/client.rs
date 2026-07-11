//! Socket protocol and client helpers for the warm-pool daemon.

use a3s_box_core::error::{BoxError, Result};
use serde::{Deserialize, Serialize};

/// Wire protocol for the `pool` Unix socket.
///
/// Client→daemon request: run a one-shot command, query status, stop the
/// daemon, or manage a short-lived leased VM session.
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PoolRequest {
    Run(PoolRunRequest),
    Status,
    Stop,
    Lease(PoolLeaseRequest),
    Exec(PoolLeaseExecRequest),
    Release(PoolLeaseReleaseRequest),
}

#[derive(Serialize, Deserialize)]
pub struct PoolRunRequest {
    /// Image to run in; `None` means use the daemon's default image.
    #[serde(default)]
    pub image: Option<String>,
    /// User to run as (uid[:gid] or name); `None` runs as the image default.
    #[serde(default)]
    pub user: Option<String>,
    /// Working directory inside the sandbox.
    #[serde(default)]
    pub workdir: Option<String>,
    /// Optional guest-visible rootfs to chroot into before executing.
    #[serde(default)]
    pub rootfs: Option<String>,
    /// Extra KEY=VALUE environment entries.
    #[serde(default)]
    pub env: Vec<String>,
    /// Boot-time volume specs for this sandbox pool.
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Boot-time vCPU count for lazily-created pools.
    #[serde(default)]
    pub vcpus: Option<u32>,
    /// Boot-time memory size for lazily-created pools.
    #[serde(default)]
    pub memory_mb: Option<u32>,
    /// Force exec mode for this request.
    #[serde(default)]
    pub exec: bool,
    /// Guest-side execution timeout in nanoseconds.
    #[serde(default)]
    pub timeout_ns: Option<u64>,
    pub cmd: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct PoolRunResponse {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct PoolLeaseRequest {
    /// Image for the helper VM; `None` means use the daemon's default image.
    #[serde(default)]
    pub image: Option<String>,
    /// Boot-time volume specs for this leased VM.
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Boot-time vCPU count.
    #[serde(default)]
    pub vcpus: Option<u32>,
    /// Boot-time memory size.
    #[serde(default)]
    pub memory_mb: Option<u32>,
}

#[derive(Serialize, Deserialize)]
pub struct PoolLeaseResponse {
    pub lease_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct PoolLeaseExecRequest {
    pub lease_id: String,
    pub cmd: Vec<String>,
    #[serde(default)]
    pub timeout_ns: Option<u64>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub rootfs: Option<String>,
    #[serde(default)]
    pub stdin: Option<Vec<u8>>,
    #[serde(default)]
    pub user: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct PoolLeaseReleaseRequest {
    pub lease_id: String,
}

#[derive(Serialize, Deserialize)]
pub struct PoolLeaseReleaseResponse {
    pub error: Option<String>,
}

/// Live stats for one image's warm pool.
#[derive(Serialize, Deserialize)]
pub struct PoolImageStat {
    pub image: String,
    pub pool: String,
    /// Maximum concurrent sandboxes for this pool key.
    #[serde(default)]
    pub max: usize,
    pub idle: usize,
    /// Sandboxes currently checked out by one-shot runs or leases.
    #[serde(default)]
    pub active: usize,
    /// Active sandboxes held by lease clients.
    #[serde(default)]
    pub leased: usize,
    pub total_created: u64,
    pub total_acquired: u64,
    pub total_evicted: u64,
}

#[derive(Serialize, Deserialize)]
pub struct PoolStatusResponse {
    pub images: Vec<PoolImageStat>,
}

#[derive(Serialize, Deserialize)]
pub struct PoolStopResponse {
    pub error: Option<String>,
}

pub struct PoolClientRun {
    pub socket: String,
    pub image: Option<String>,
    pub user: Option<String>,
    pub workdir: Option<String>,
    pub rootfs: Option<String>,
    pub env: Vec<String>,
    pub volumes: Vec<String>,
    pub vcpus: u32,
    pub memory_mb: u32,
    pub exec: bool,
    pub timeout_ns: Option<u64>,
    pub cmd: Vec<String>,
}

pub struct PoolClientOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
}

pub struct PoolLeaseClient {
    socket: String,
    lease_id: String,
    released: bool,
}

impl PoolLeaseClient {
    pub fn lease_id(&self) -> &str {
        &self.lease_id
    }

    pub async fn acquire(req: PoolClientLease) -> Result<Self> {
        let response = lease_client(&req).await?;
        let lease_id = response.lease_id.ok_or_else(|| {
            BoxError::PoolError("pool lease response did not include a lease id".to_string())
        })?;
        Ok(Self {
            socket: req.socket,
            lease_id,
            released: false,
        })
    }

    pub async fn exec(&self, req: PoolLeaseExec) -> Result<PoolClientOutput> {
        lease_exec_client(
            &self.socket,
            PoolLeaseExecRequest {
                lease_id: self.lease_id.clone(),
                cmd: req.cmd,
                timeout_ns: req.timeout_ns,
                env: req.env,
                working_dir: req.working_dir,
                rootfs: req.rootfs,
                stdin: req.stdin,
                user: req.user,
            },
        )
        .await
    }

    pub async fn release(mut self) -> Result<()> {
        let result = release_client(&self.socket, &self.lease_id).await;
        if result.is_ok() {
            self.released = true;
        }
        result
    }
}

impl Drop for PoolLeaseClient {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        #[cfg(not(windows))]
        release_client_blocking_best_effort(&self.socket, &self.lease_id);
    }
}

pub struct PoolClientLease {
    pub socket: String,
    pub image: Option<String>,
    pub volumes: Vec<String>,
    pub vcpus: u32,
    pub memory_mb: u32,
}

pub struct PoolLeaseExec {
    pub cmd: Vec<String>,
    pub timeout_ns: Option<u64>,
    pub env: Vec<String>,
    pub working_dir: Option<String>,
    pub rootfs: Option<String>,
    pub stdin: Option<Vec<u8>>,
    pub user: Option<String>,
}

#[cfg(not(windows))]
pub async fn run_client(req: PoolClientRun) -> Result<PoolClientOutput> {
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(&req.socket).await.map_err(|e| {
        BoxError::PoolError(format!(
            "Failed to connect to pool daemon at {} ({}). Is `a3s-box pool start` running?",
            req.socket, e
        ))
    })?;

    write_frame(
        &mut stream,
        &serde_json::to_vec(&PoolRequest::Run(PoolRunRequest {
            image: req.image,
            user: req.user,
            workdir: req.workdir,
            rootfs: req.rootfs,
            env: req.env,
            volumes: req.volumes,
            vcpus: Some(req.vcpus),
            memory_mb: Some(req.memory_mb),
            exec: req.exec,
            timeout_ns: req.timeout_ns,
            cmd: req.cmd,
        }))?,
    )
    .await?;
    let resp: PoolRunResponse = serde_json::from_slice(&read_frame(&mut stream).await?)?;

    if let Some(err) = resp.error {
        return Err(BoxError::PoolError(err));
    }

    Ok(PoolClientOutput {
        stdout: resp.stdout,
        stderr: resp.stderr,
        exit_code: resp.exit_code,
    })
}

#[cfg(windows)]
pub async fn run_client(_req: PoolClientRun) -> Result<PoolClientOutput> {
    Err(BoxError::PoolError(
        "`pool run` is not supported on Windows".to_string(),
    ))
}

#[cfg(not(windows))]
pub async fn status_client(socket: &str) -> Result<PoolStatusResponse> {
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(socket).await.map_err(|e| {
        BoxError::PoolError(format!("Failed to connect to pool daemon at {socket}: {e}"))
    })?;
    write_frame(&mut stream, &serde_json::to_vec(&PoolRequest::Status)?).await?;
    Ok(serde_json::from_slice(&read_frame(&mut stream).await?)?)
}

#[cfg(not(windows))]
pub async fn stop_client(socket: &str) -> Result<()> {
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(socket).await.map_err(|e| {
        BoxError::PoolError(format!("Failed to connect to pool daemon at {socket}: {e}"))
    })?;
    write_frame(&mut stream, &serde_json::to_vec(&PoolRequest::Stop)?).await?;
    let resp: PoolStopResponse = serde_json::from_slice(&read_frame(&mut stream).await?)?;
    if let Some(error) = resp.error {
        return Err(BoxError::PoolError(error));
    }
    Ok(())
}

#[cfg(windows)]
pub async fn stop_client(_socket: &str) -> Result<()> {
    Err(BoxError::PoolError(
        "`pool stop` is not supported on Windows".to_string(),
    ))
}

#[cfg(not(windows))]
async fn lease_client(req: &PoolClientLease) -> Result<PoolLeaseResponse> {
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(&req.socket).await.map_err(|e| {
        BoxError::PoolError(format!(
            "Failed to connect to pool daemon at {} ({}). Is `a3s-box pool start` running?",
            req.socket, e
        ))
    })?;
    write_frame(
        &mut stream,
        &serde_json::to_vec(&PoolRequest::Lease(PoolLeaseRequest {
            image: req.image.clone(),
            volumes: req.volumes.clone(),
            vcpus: Some(req.vcpus),
            memory_mb: Some(req.memory_mb),
        }))?,
    )
    .await?;
    let resp: PoolLeaseResponse = serde_json::from_slice(&read_frame(&mut stream).await?)?;
    if let Some(error) = resp.error.as_ref() {
        return Err(BoxError::PoolError(error.clone()));
    }
    Ok(resp)
}

#[cfg(windows)]
async fn lease_client(_req: &PoolClientLease) -> Result<PoolLeaseResponse> {
    Err(BoxError::PoolError(
        "warm-pool leases are not supported on Windows".to_string(),
    ))
}

#[cfg(not(windows))]
async fn lease_exec_client(socket: &str, req: PoolLeaseExecRequest) -> Result<PoolClientOutput> {
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(socket).await.map_err(|e| {
        BoxError::PoolError(format!("Failed to connect to pool daemon at {socket}: {e}"))
    })?;
    write_frame(&mut stream, &serde_json::to_vec(&PoolRequest::Exec(req))?).await?;
    let resp: PoolRunResponse = serde_json::from_slice(&read_frame(&mut stream).await?)?;
    if let Some(error) = resp.error {
        return Err(BoxError::PoolError(error));
    }
    Ok(PoolClientOutput {
        stdout: resp.stdout,
        stderr: resp.stderr,
        exit_code: resp.exit_code,
    })
}

#[cfg(windows)]
async fn lease_exec_client(_socket: &str, _req: PoolLeaseExecRequest) -> Result<PoolClientOutput> {
    Err(BoxError::PoolError(
        "warm-pool leases are not supported on Windows".to_string(),
    ))
}

#[cfg(not(windows))]
async fn release_client(socket: &str, lease_id: &str) -> Result<()> {
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(socket).await.map_err(|e| {
        BoxError::PoolError(format!("Failed to connect to pool daemon at {socket}: {e}"))
    })?;
    write_frame(
        &mut stream,
        &serde_json::to_vec(&PoolRequest::Release(PoolLeaseReleaseRequest {
            lease_id: lease_id.to_string(),
        }))?,
    )
    .await?;
    let resp: PoolLeaseReleaseResponse = serde_json::from_slice(&read_frame(&mut stream).await?)?;
    if let Some(error) = resp.error {
        return Err(BoxError::PoolError(error));
    }
    Ok(())
}

#[cfg(windows)]
async fn release_client(_socket: &str, _lease_id: &str) -> Result<()> {
    Err(BoxError::PoolError(
        "warm-pool leases are not supported on Windows".to_string(),
    ))
}

#[cfg(not(windows))]
fn release_client_blocking_best_effort(socket: &str, lease_id: &str) {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let Ok(mut stream) = UnixStream::connect(socket) else {
        return;
    };
    let timeout = Some(Duration::from_millis(500));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);

    let Ok(payload) = serde_json::to_vec(&PoolRequest::Release(PoolLeaseReleaseRequest {
        lease_id: lease_id.to_string(),
    })) else {
        return;
    };
    let _ = stream
        .write_all(&(payload.len() as u32).to_le_bytes())
        .and_then(|_| stream.write_all(&payload))
        .and_then(|_| stream.flush());
}

/// Length-prefixed (u32 LE) framing for the pool Unix-socket protocol.
#[cfg(not(windows))]
pub async fn write_frame<W>(w: &mut W, data: &[u8]) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    w.write_all(&(data.len() as u32).to_le_bytes()).await?;
    w.write_all(data).await?;
    w.flush().await
}

#[cfg(not(windows))]
pub async fn read_frame<R>(r: &mut R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    #[cfg(not(windows))]
    #[test]
    fn lease_drop_releases_synchronously() {
        use super::*;
        use std::io::Read;
        use std::os::unix::net::UnixListener;

        let tmp = tempfile::TempDir::new().unwrap();
        let socket = tmp.path().join("pool.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let socket_arg = socket.to_string_lossy().to_string();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut len = [0_u8; 4];
            stream.read_exact(&mut len).unwrap();
            let mut request = vec![0_u8; u32::from_le_bytes(len) as usize];
            stream.read_exact(&mut request).unwrap();
            let request: PoolRequest = serde_json::from_slice(&request).unwrap();
            match request {
                PoolRequest::Release(req) => assert_eq!(req.lease_id, "lease-drop"),
                _ => panic!("drop should send release request"),
            }
        });

        let lease = PoolLeaseClient {
            socket: socket_arg,
            lease_id: "lease-drop".to_string(),
            released: false,
        };
        drop(lease);

        server.join().unwrap();
    }
}
