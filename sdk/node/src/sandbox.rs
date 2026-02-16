use std::sync::Arc;

use napi::Result;
use napi_derive::napi;
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

use a3s_box_sdk::Sandbox;

use crate::types::{JsExecMetrics, JsExecResult};

fn to_napi_error(e: impl std::fmt::Display) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// A running MicroVM sandbox.
#[napi]
pub struct JsSandbox {
    inner: Arc<Mutex<Option<Sandbox>>>,
    runtime: Arc<Runtime>,
    id: String,
    name: String,
}

impl JsSandbox {
    pub fn new(sandbox: Sandbox, runtime: Arc<Runtime>) -> Self {
        let id = sandbox.id().to_string();
        let name = sandbox.name().to_string();
        Self {
            inner: Arc::new(Mutex::new(Some(sandbox))),
            runtime,
            id,
            name,
        }
    }
}

#[napi]
impl JsSandbox {
    /// Get the sandbox ID.
    #[napi(getter)]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get the sandbox name.
    #[napi(getter)]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Execute a command and wait for completion.
    #[napi]
    pub fn exec(
        &self,
        cmd: String,
        args: Option<Vec<String>>,
        env: Option<Vec<String>>,
        workdir: Option<String>,
    ) -> Result<JsExecResult> {
        let inner = self.inner.clone();

        self.runtime.block_on(async {
            let guard = inner.lock().await;
            let sandbox = guard
                .as_ref()
                .ok_or_else(|| napi::Error::from_reason("Sandbox already stopped"))?;

            let result = if env.is_some() || workdir.is_some() {
                let mut cmd_parts = vec![cmd];
                cmd_parts.extend(args.unwrap_or_default());
                sandbox
                    .exec_with_options(cmd_parts, env.unwrap_or_default(), workdir, None)
                    .await
            } else {
                let arg_refs: Vec<&str> = args
                    .as_ref()
                    .map(|a| a.iter().map(|s| s.as_str()).collect())
                    .unwrap_or_default();
                sandbox.exec(&cmd, &arg_refs).await
            };

            let result = result.map_err(to_napi_error)?;

            Ok(JsExecResult {
                stdout: result.stdout,
                stderr: result.stderr,
                exit_code: result.exit_code,
                metrics: JsExecMetrics {
                    duration_ms: result.metrics.duration_ms as u32,
                    stdout_bytes: result.metrics.stdout_bytes as u32,
                    stderr_bytes: result.metrics.stderr_bytes as u32,
                },
            })
        })
    }

    /// Upload a file into the sandbox.
    #[napi]
    pub fn upload(&self, data: napi::bindgen_prelude::Buffer, guest_path: String) -> Result<()> {
        let inner = self.inner.clone();

        self.runtime.block_on(async {
            let guard = inner.lock().await;
            let sandbox = guard
                .as_ref()
                .ok_or_else(|| napi::Error::from_reason("Sandbox already stopped"))?;

            sandbox
                .upload(&data, &guest_path)
                .await
                .map_err(to_napi_error)
        })
    }

    /// Download a file from the sandbox.
    #[napi]
    pub fn download(&self, guest_path: String) -> Result<napi::bindgen_prelude::Buffer> {
        let inner = self.inner.clone();

        self.runtime.block_on(async {
            let guard = inner.lock().await;
            let sandbox = guard
                .as_ref()
                .ok_or_else(|| napi::Error::from_reason("Sandbox already stopped"))?;

            let data = sandbox.download(&guest_path).await.map_err(to_napi_error)?;
            Ok(data.into())
        })
    }

    /// Check if the sandbox is running.
    #[napi]
    pub fn is_running(&self) -> Result<bool> {
        let inner = self.inner.clone();

        self.runtime.block_on(async {
            let guard = inner.lock().await;
            match guard.as_ref() {
                Some(sandbox) => Ok(sandbox.is_running().await),
                None => Ok(false),
            }
        })
    }

    /// Stop the sandbox and release resources.
    #[napi]
    pub fn stop(&self) -> Result<()> {
        let inner = self.inner.clone();

        self.runtime.block_on(async {
            let mut guard = inner.lock().await;
            if let Some(sandbox) = guard.take() {
                sandbox.stop().await.map_err(to_napi_error)?;
            }
            Ok(())
        })
    }
}
