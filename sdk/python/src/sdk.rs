use pyo3::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Runtime;

use a3s_box_sdk::BoxSdk;

use crate::sandbox::PySandbox;
use crate::types::PySandboxOptions;

/// SDK entry point for creating and managing MicroVM sandboxes.
#[pyclass]
pub struct PyBoxSdk {
    inner: BoxSdk,
    runtime: Arc<Runtime>,
}

#[pymethods]
impl PyBoxSdk {
    /// Create a new BoxSdk instance.
    #[new]
    #[pyo3(signature = (home_dir=None))]
    fn new(py: Python<'_>, home_dir: Option<String>) -> PyResult<Self> {
        let runtime = Arc::new(
            Runtime::new()
                .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?,
        );

        let inner = py.allow_threads(|| {
            runtime.block_on(async {
                if let Some(dir) = home_dir {
                    BoxSdk::with_home(PathBuf::from(dir)).await
                } else {
                    BoxSdk::new().await
                }
            })
        })
        .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

        Ok(Self { inner, runtime })
    }

    /// Get the SDK home directory.
    #[getter]
    fn home_dir(&self) -> String {
        self.inner.home_dir().display().to_string()
    }

    /// Create a new sandbox.
    #[pyo3(signature = (options=None))]
    fn create(&self, py: Python<'_>, options: Option<PySandboxOptions>) -> PyResult<PySandbox> {
        let opts = match options {
            Some(ref o) => a3s_box_sdk::SandboxOptions::from(o),
            None => a3s_box_sdk::SandboxOptions::default(),
        };

        let runtime = self.runtime.clone();
        let sandbox = py.allow_threads(|| {
            runtime.block_on(async {
                self.inner.create(opts).await
            })
        })
        .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

        Ok(PySandbox::new(sandbox, self.runtime.clone()))
    }

    fn __repr__(&self) -> String {
        format!("BoxSdk(home='{}')", self.inner.home_dir().display())
    }
}
