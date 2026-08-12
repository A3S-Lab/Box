use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use a3s_box_core::{CompiledEgressPolicy, EgressPolicyLimits, EgressProtocol};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;

use super::decision_log::{EgressDecisionLog, EgressRuntimeDecisionReason};
use super::proxy::EgressProxyError;

const MAX_POLICY_QUERY_BYTES: usize = 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EgressPolicyQuery {
    protocol: EgressProtocol,
    address: IpAddr,
    port: u16,
}

#[derive(Debug, Serialize)]
struct EgressPolicyResponse {
    allowed: bool,
}

pub(super) async fn bind(path: &Path) -> std::io::Result<UnixListener> {
    prepare_parent(path)?;
    let listener = UnixListener::bind(path)?;
    if let Err(error) = secure_socket(path) {
        drop(listener);
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(listener)
}

pub(super) async fn serve(
    listener: UnixListener,
    policy: Arc<CompiledEgressPolicy>,
    decision_log: Arc<EgressDecisionLog>,
    limits: EgressPolicyLimits,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), EgressProxyError> {
    let capacity = Arc::new(Semaphore::new(limits.max_pending_connections as usize));
    let timeout = Duration::from_millis(u64::from(limits.connect_timeout_ms));
    let mut tasks = JoinSet::new();
    let result = loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break Ok(());
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => break Err(EgressProxyError::Io(error)),
                };
                let permit = match Arc::clone(&capacity).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        let _ = decision_log
                            .append_runtime_denied(
                                EgressRuntimeDecisionReason::PendingConnectionLimitExceeded,
                                None,
                                None,
                                None,
                            )
                            .await;
                        continue;
                    }
                };
                let task_policy = Arc::clone(&policy);
                let task_log = Arc::clone(&decision_log);
                tasks.spawn(async move {
                    let _permit = permit;
                    handle(stream, task_policy, task_log, timeout).await;
                });
            }
            Some(completed) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(error) = completed {
                    tracing::debug!(%error, "egress policy query task terminated");
                }
            }
        }
    };

    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    decision_log.finish().await?;
    result
}

pub(super) async fn handle(
    mut stream: UnixStream,
    policy: Arc<CompiledEgressPolicy>,
    decision_log: Arc<EgressDecisionLog>,
    timeout: Duration,
) {
    let query = match tokio::time::timeout(timeout, read_query(&mut stream)).await {
        Ok(Ok(query)) => query,
        Ok(Err(_)) | Err(_) => {
            let _ = decision_log
                .append_runtime_denied(
                    EgressRuntimeDecisionReason::MalformedPolicyQuery,
                    None,
                    None,
                    None,
                )
                .await;
            let _ = write_response(&mut stream, false).await;
            return;
        }
    };

    let evaluation = policy.evaluate_ip(query.protocol, query.address, query.port);
    let policy_allowed = evaluation.is_allowed();
    let allowed = decision_log.append_policy(evaluation).await.is_ok() && policy_allowed;
    let _ = write_response(&mut stream, allowed).await;
}

pub(super) fn remove_socket(path: Option<&Path>) {
    let Some(path) = path else {
        return;
    };
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "failed to remove egress policy socket");
        }
    }
}

async fn read_query(stream: &mut UnixStream) -> std::io::Result<EgressPolicyQuery> {
    let mut bytes = Vec::with_capacity(128);
    loop {
        let mut chunk = [0_u8; 256];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "egress policy query ended before newline",
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > MAX_POLICY_QUERY_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "egress policy query exceeded its byte limit",
            ));
        }
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            if newline + 1 != bytes.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "egress policy channel accepts one query per connection",
                ));
            }
            bytes.truncate(newline);
            break;
        }
    }

    serde_json::from_slice(&bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

async fn write_response(stream: &mut UnixStream, allowed: bool) -> std::io::Result<()> {
    let mut encoded =
        serde_json::to_vec(&EgressPolicyResponse { allowed }).map_err(std::io::Error::other)?;
    encoded.push(b'\n');
    stream.write_all(&encoded).await?;
    stream.shutdown().await
}

fn prepare_parent(path: &Path) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("egress policy socket path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let metadata = std::fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "egress policy socket parent is not a plain directory",
        ));
    }

    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // SAFETY: querying the effective process UID has no preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "egress policy socket directory is not owned by the runtime user",
        ));
    }
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
}

fn secure_socket(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use a3s_box_core::{EgressIpRule, EgressPolicy, ExecutionGeneration, ExecutionId};
    use tokio::net::UnixStream;

    use super::*;

    #[tokio::test]
    async fn policy_channel_is_no_clobber_owner_only_and_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("policy.sock");
        let log_path = directory.path().join("decisions.jsonl");
        let policy = EgressPolicy::allowlist([], [EgressIpRule::tcp("127.0.0.1/32", 443)]);
        let compiled = Arc::new(CompiledEgressPolicy::compile(&policy).unwrap());
        let log = Arc::new(
            EgressDecisionLog::create(
                &log_path,
                ExecutionId::new("policy-channel-test").unwrap(),
                ExecutionGeneration::INITIAL,
                compiled.limits(),
            )
            .await
            .unwrap(),
        );
        let listener = bind(&socket_path).await.unwrap();
        assert!(bind(&socket_path).await.is_err());

        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (stream, _) = listener.accept().await.unwrap();
                handle(
                    stream,
                    Arc::clone(&compiled),
                    Arc::clone(&log),
                    Duration::from_secs(1),
                )
                .await;
            }
            log.finish().await.unwrap();
        });

        assert!(query(&socket_path, "tcp", "127.0.0.1", 443).await);
        assert!(!query(&socket_path, "tcp", "127.0.0.1", 80).await);

        let mut malformed = UnixStream::connect(&socket_path).await.unwrap();
        malformed.write_all(b"{}\ntrailing").await.unwrap();
        let mut response = String::new();
        malformed.read_to_string(&mut response).await.unwrap();
        assert_eq!(response, "{\"allowed\":false}\n");

        server.await.unwrap();
        let content = tokio::fs::read_to_string(log_path).await.unwrap();
        assert!(content.contains("ip_rule_matched"));
        assert!(content.contains("ip_not_allowed"));
        assert!(content.contains("malformed_policy_query"));

        let mode = std::fs::symlink_metadata(&socket_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        remove_socket(Some(&socket_path));
        assert!(!socket_path.exists());
    }

    async fn query(path: &Path, protocol: &str, address: &str, port: u16) -> bool {
        let mut stream = UnixStream::connect(path).await.unwrap();
        stream
            .write_all(
                format!(
                    "{{\"protocol\":\"{protocol}\",\"address\":\"{address}\",\"port\":{port}}}\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        serde_json::from_str::<serde_json::Value>(response.trim()).unwrap()["allowed"]
            .as_bool()
            .unwrap()
    }
}
