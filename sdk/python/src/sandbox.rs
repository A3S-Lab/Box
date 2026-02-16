use pyo3::prelude::*;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

use a3s_box_sdk::Sandbox;

use crate::types::{PyExecMetrics, PyExecResult};

/// A running MicroVM sandbox.
#[pyclass]
pub struct PySandbox {
    inner: Arc<Mutex<Option<Sandbox>>>,
    runtime: Arc<Runtime>,
    id: String,
    name: String,
}

impl PySandbox {
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

#[pymethods]
impl PySandbox {
    /// Get the sandbox ID.
    #[getter]
    fn id(&self) -> &str {
        &self.id
    }

    /// Get the sandbox name.
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    /// Execute a command and wait for completion.
    #[pyo3(signature = (*args, env=None, workdir=None))]
    fn exec(
        &self,
        py: Python<'_>,
        args: Vec<String>,
        env: Option<Vec<String>>,
        workdir: Option<String>,
    ) -> PyResult<PyExecResult> {
        if args.is_empty() {
            return Err(pyo3::exceptions::PyValueError::new_err("Command cannot be empty"));
        }

        let inner = self.inner.clone();
        let runtime = self.runtime.clone();

        py.allow_threads(move || {
            runtime.block_on(async {
                let guard = inner.lock().await;
                let sandbox = guard.as_ref().ok_or_else(|| {
                    pyo3::exceptions::PyRuntimeError::new_err("Sandbox already stopped")
                })?;

                let result = if env.is_some() || workdir.is_some() {
                    sandbox
                        .exec_with_options(
                            args,
                            env.unwrap_or_default(),
                            workdir,
                            None,
                        )
                        .await
                } else {
                    let cmd = &args[0];
                    let cmd_args: Vec<&str> = args[1..].iter().map(|s| s.as_str()).collect();
                    sandbox.exec(cmd, &cmd_args).await
                };

                let result = result.map_err(|e| {
                    pyo3::exceptions::PyRuntimeError::new_err(e.to_string())
                })?;

                Ok(PyExecResult {
                    stdout: result.stdout,
                    stderr: result.stderr,
                    exit_code: result.exit_code,
                    metrics: PyExecMetrics::from(result.metrics),
                })
            })
        })
    }

    /// Upload a file into the sandbox.
    fn upload(&self, py: Python<'_>, data: Vec<u8>, guest_path: String) -> PyResult<()> {
        let inner = self.inner.clone();
        let runtime = self.runtime.clone();

        py.allow_threads(move || {
            runtime.block_on(async {
                let guard = inner.lock().await;
                let sandbox = guard.as_ref().ok_or_else(|| {
                    pyo3::exceptions::PyRuntimeError::new_err("Sandbox already stopped")
                })?;

                sandbox.upload(&data, &guest_path).await.map_err(|e| {
                    pyo3::exceptions::PyRuntimeError::new_err(e.to_string())
                })
            })
        })
    }

    /// Download a file from the sandbox.
    fn download(&self, py: Python<'_>, guest_path: String) -> PyResult<Vec<u8>> {
        let inner = self.inner.clone();
        let runtime = self.runtime.clone();

        py.allow_threads(move || {
            runtime.block_on(async {
                let guard = inner.lock().await;
                let sandbox = guard.as_ref().ok_or_else(|| {
                    pyo3::exceptions::PyRuntimeError::new_err("Sandbox already stopped")
                })?;

                sandbox.download(&guest_path).await.map_err(|e| {
                    pyo3::exceptions::PyRuntimeError::new_err(e.to_string())
                })
            })
        })
    }

    /// Check if the sandbox is running.
    fn is_running(&self, py: Python<'_>) -> PyResult<bool> {
        let inner = self.inner.clone();
        let runtime = self.runtime.clone();

        py.allow_threads(move || {
            runtime.block_on(async {
                let guard = inner.lock().await;
                match guard.as_ref() {
                    Some(sandbox) => Ok(sandbox.is_running().await),
                    None => Ok(false),
                }
            })
        })
    }

    /// Stop the sandbox and release resources.
    fn stop(&self, py: Python<'_>) -> PyResult<()> {
        let inner = self.inner.clone();
        let runtime = self.runtime.clone();

        py.allow_threads(move || {
            runtime.block_on(async {
                let mut guard = inner.lock().await;
                if let Some(sandbox) = guard.take() {
                    sandbox.stop().await.map_err(|e| {
                        pyo3::exceptions::PyRuntimeError::new_err(e.to_string())
                    })?;
                }
                Ok(())
            })
        })
    }

    fn __repr__(&self) -> String {
        format!("Sandbox(id='{}', name='{}')", self.id, self.name)
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (_exc_type=None, _exc_val=None, _exc_tb=None))]
    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: Option<PyObject>,
        _exc_val: Option<PyObject>,
        _exc_tb: Option<PyObject>,
    ) -> PyResult<bool> {
        self.stop(py)?;
        Ok(false)
    }
}
