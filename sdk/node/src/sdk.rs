use std::path::PathBuf;
use std::sync::Arc;

use napi::Result;
use napi_derive::napi;
use tokio::runtime::Runtime;

use a3s_box_sdk::BoxSdk;

use crate::sandbox::JsSandbox;
use crate::types::JsSandboxOptions;

fn to_napi_error(e: impl std::fmt::Display) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// SDK entry point for creating and managing MicroVM sandboxes.
#[napi]
pub struct JsBoxSdk {
    inner: BoxSdk,
    runtime: Arc<Runtime>,
}

#[napi]
impl JsBoxSdk {
    /// Create a new BoxSdk instance.
    #[napi(constructor)]
    pub fn new(home_dir: Option<String>) -> Result<Self> {
        let runtime = Arc::new(Runtime::new().map_err(to_napi_error)?);

        let inner = runtime
            .block_on(async {
                if let Some(dir) = home_dir {
                    BoxSdk::with_home(PathBuf::from(dir)).await
                } else {
                    BoxSdk::new().await
                }
            })
            .map_err(to_napi_error)?;

        Ok(Self { inner, runtime })
    }

    /// Get the SDK home directory.
    #[napi(getter)]
    pub fn home_dir(&self) -> String {
        self.inner.home_dir().display().to_string()
    }

    /// Create a new sandbox.
    #[napi]
    pub fn create(&self, options: Option<JsSandboxOptions>) -> Result<JsSandbox> {
        let opts = match options {
            Some(ref o) => a3s_box_sdk::SandboxOptions::from(o),
            None => a3s_box_sdk::SandboxOptions::default(),
        };

        let sandbox = self
            .runtime
            .block_on(self.inner.create(opts))
            .map_err(to_napi_error)?;

        Ok(JsSandbox::new(sandbox, self.runtime.clone()))
    }
}
