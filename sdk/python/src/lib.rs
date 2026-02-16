use pyo3::prelude::*;

mod sandbox;
mod sdk;
mod types;

use sandbox::PySandbox;
use sdk::PyBoxSdk;
use types::{PyExecMetrics, PyExecResult, PyMountSpec, PyPortForward, PySandboxOptions, PyWorkspaceConfig};

/// Native Python bindings for A3S Box MicroVM sandbox runtime.
#[pymodule]
fn _a3s_box(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyBoxSdk>()?;
    m.add_class::<PySandbox>()?;
    m.add_class::<PySandboxOptions>()?;
    m.add_class::<PyExecResult>()?;
    m.add_class::<PyExecMetrics>()?;
    m.add_class::<PyMountSpec>()?;
    m.add_class::<PyPortForward>()?;
    m.add_class::<PyWorkspaceConfig>()?;
    Ok(())
}
