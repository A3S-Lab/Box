//! Guest one-shot exec identity for CRI non-streaming paths.
//!
//! The guest journals keyed one-shot exec before the response write. CRI
//! `ExecSync` and non-interactive streaming oneshot keep the sandbox VM alive,
//! so minting a `cri-exec-*` id and retrying once on ambiguous transport loss
//! reuses that journal instead of re-running the command. Streaming / SPDY /
//! stdin sessions stay unkeyed (guest rejects `request_id` on streaming).

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::exec::{ExecOutput, ExecRequest};

/// Mint a durable process-journal identity for one CRI one-shot exec.
pub fn mint_cri_exec_request_id() -> String {
    format!("cri-exec-{}", uuid::Uuid::new_v4().simple())
}

/// True when the failure may have occurred after the guest already claimed or
/// completed the keyed exec (lost response / broken connection).
pub fn is_ambiguous_exec_transport(error: &BoxError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("unavailable")
        || message.contains("closed without response")
        || message.contains("response timed out")
        || message.contains("response read failed")
        || message.contains("connection failed")
}

/// Run a keyed one-shot exec once, and retry once with the **same**
/// `request_id` after an ambiguous transport failure.
pub async fn exec_with_guest_replay_retry<F, Fut>(
    request: ExecRequest,
    mut exec: F,
) -> Result<ExecOutput>
where
    F: FnMut(ExecRequest) -> Fut,
    Fut: std::future::Future<Output = Result<ExecOutput>>,
{
    let can_replay = request.request_id.is_some();
    match exec(request.clone()).await {
        Ok(output) => Ok(output),
        Err(error) if can_replay && is_ambiguous_exec_transport(&error) => exec(request).await,
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn mint_cri_exec_request_id_uses_stable_prefix() {
        let minted = mint_cri_exec_request_id();
        assert!(minted.starts_with("cri-exec-"), "{minted}");
        assert!(!minted.contains('\0'));
        assert!(minted.len() <= 512);
    }

    #[test]
    fn ambiguous_transport_detects_lost_response_and_connection() {
        assert!(is_ambiguous_exec_transport(&BoxError::ExecError(
            "Exec server closed without response".to_string()
        )));
        assert!(is_ambiguous_exec_transport(&BoxError::ExecError(
            "Exec response timed out after 15s".to_string()
        )));
        assert!(is_ambiguous_exec_transport(&BoxError::ExecError(
            "Exec connection failed to /tmp/x".to_string()
        )));
        assert!(!is_ambiguous_exec_transport(&BoxError::ExecError(
            "command rejected by guest policy".to_string()
        )));
    }

    #[tokio::test]
    async fn guest_replay_retry_reuses_same_request_once() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_clone = attempts.clone();
        let request = ExecRequest {
            request_id: Some("cri-exec-stable-1".to_string()),
            cmd: vec!["true".to_string()],
            timeout_ns: 1_000_000_000,
            env: vec![],
            working_dir: None,
            rootfs: None,
            stdin: None,
            stdin_streaming: false,
            user: None,
            streaming: false,
        };

        let output = exec_with_guest_replay_retry(request, move |req| {
            let attempts = attempts_clone.clone();
            async move {
                assert_eq!(req.request_id.as_deref(), Some("cri-exec-stable-1"));
                let n = attempts.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(BoxError::ExecError(
                        "Exec server closed without response".to_string(),
                    ))
                } else {
                    Ok(ExecOutput {
                        stdout: b"ok".to_vec(),
                        stderr: Vec::new(),
                        exit_code: 0,
                        truncated: false,
                    })
                }
            }
        })
        .await
        .expect("retry recovers");

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, b"ok");
    }

    #[tokio::test]
    async fn guest_replay_retry_skips_non_ambiguous_errors() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_clone = attempts.clone();
        let request = ExecRequest {
            request_id: Some("cri-exec-stable-2".to_string()),
            cmd: vec!["true".to_string()],
            timeout_ns: 1_000_000_000,
            env: vec![],
            working_dir: None,
            rootfs: None,
            stdin: None,
            stdin_streaming: false,
            user: None,
            streaming: false,
        };

        let error = exec_with_guest_replay_retry(request, move |_| {
            let attempts = attempts_clone.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err(BoxError::ExecError(
                    "command rejected by guest policy".to_string(),
                ))
            }
        })
        .await
        .expect_err("non-ambiguous must not retry");

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(error.to_string().contains("guest policy"));
    }
}
