// Suppress warning from pyo3 macro expansion (will be fixed in newer pyo3 versions)
#![allow(non_local_definitions)]

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyModule};

/// Python module for A3S Box
#[pymodule]
fn a3s_box(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<Box>()?;
    m.add_class::<Session>()?;
    m.add_class::<ModelConfig>()?;
    m.add_class::<ResourceConfig>()?;
    m.add_class::<LaneConfig>()?;
    m.add_class::<GenerateResult>()?;
    m.add_class::<TokenUsage>()?;
    m.add_function(wrap_pyfunction!(create_box, m)?)?;

    // Add version
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;

    Ok(())
}

/// Box configuration
#[pyclass]
#[derive(Clone)]
struct ModelConfig {
    #[pyo3(get, set)]
    provider: String,

    #[pyo3(get, set)]
    name: String,

    #[pyo3(get, set)]
    base_url: Option<String>,

    #[pyo3(get, set)]
    api_key: Option<String>,
}

#[pymethods]
impl ModelConfig {
    #[new]
    #[pyo3(signature = (provider="anthropic".to_string(), name="claude-sonnet-4-20250514".to_string(), base_url=None, api_key=None))]
    fn new(
        provider: String,
        name: String,
        base_url: Option<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            provider,
            name,
            base_url,
            api_key,
        }
    }
}

/// Resource configuration
#[pyclass]
#[derive(Clone)]
struct ResourceConfig {
    #[pyo3(get, set)]
    vcpus: u32,

    #[pyo3(get, set)]
    memory_mb: u32,

    #[pyo3(get, set)]
    disk_mb: u32,

    #[pyo3(get, set)]
    timeout: u64,
}

#[pymethods]
impl ResourceConfig {
    #[new]
    #[pyo3(signature = (vcpus=2, memory_mb=1024, disk_mb=4096, timeout=3600))]
    fn new(vcpus: u32, memory_mb: u32, disk_mb: u32, timeout: u64) -> Self {
        Self {
            vcpus,
            memory_mb,
            disk_mb,
            timeout,
        }
    }
}

/// Lane configuration
#[pyclass]
#[derive(Clone)]
struct LaneConfig {
    #[pyo3(get, set)]
    min_concurrency: usize,

    #[pyo3(get, set)]
    max_concurrency: usize,
}

#[pymethods]
impl LaneConfig {
    #[new]
    fn new(min_concurrency: usize, max_concurrency: usize) -> Self {
        Self {
            min_concurrency,
            max_concurrency,
        }
    }
}

/// Box - main entry point
#[pyclass]
struct Box {
    // TODO: Add actual BoxLite runtime handle
    _placeholder: (),
}

#[pymethods]
impl Box {
    /// Create a new box
    #[new]
    #[pyo3(signature = (workspace=None, skills=None, model=None, resources=None, lanes=None, log_level=None, debug_grpc=false))]
    fn new(
        workspace: Option<String>,
        skills: Option<Vec<String>>,
        model: Option<ModelConfig>,
        resources: Option<ResourceConfig>,
        lanes: Option<&PyDict>,
        log_level: Option<String>,
        debug_grpc: bool,
    ) -> PyResult<Self> {
        // TODO: Implement box creation
        // 1. Create BoxConfig from parameters
        // 2. Initialize VmManager
        // 3. Boot VM (lazy)
        let _ = (workspace, skills, model, resources, lanes, log_level, debug_grpc);

        Ok(Self {
            _placeholder: (),
        })
    }

    /// Create a session
    #[pyo3(signature = (_system=None, _context_threshold=0.75, _context_strategy="summarize".to_string()))]
    fn create_session(
        &self,
        _system: Option<String>,
        _context_threshold: f32,
        _context_strategy: String,
    ) -> PyResult<Session> {
        // TODO: Implement session creation
        Ok(Session {
            session_id: "placeholder".to_string(),
        })
    }

    /// List sessions
    fn list_sessions(&self) -> PyResult<Vec<String>> {
        // TODO: Implement
        Ok(vec![])
    }

    /// Destroy the box
    fn destroy(&self) -> PyResult<()> {
        // TODO: Implement VM destruction
        Ok(())
    }

    /// Get queue status
    fn queue_status(&self) -> PyResult<PyObject> {
        // TODO: Implement
        Python::with_gil(|py| Ok(PyDict::new(py).into()))
    }

    /// Get metrics
    fn metrics(&self) -> PyResult<PyObject> {
        // TODO: Implement
        Python::with_gil(|py| Ok(PyDict::new(py).into()))
    }
}

/// Session
#[pyclass]
struct Session {
    #[pyo3(get)]
    session_id: String,
}

#[pymethods]
impl Session {
    /// Generate (non-streaming)
    fn generate(&self, _prompt: String) -> PyResult<GenerateResult> {
        // TODO: Implement
        Ok(GenerateResult {
            text: "placeholder".to_string(),
            usage: TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            },
        })
    }

    /// Stream (streaming)
    fn stream(&self, _prompt: String) -> PyResult<()> {
        // TODO: Implement streaming
        Ok(())
    }

    /// Generate object (structured output)
    fn generate_object(&self, _prompt: String, _schema: &PyAny) -> PyResult<PyObject> {
        // TODO: Implement
        Python::with_gil(|py| Ok(PyDict::new(py).into()))
    }

    /// Use skill
    fn use_skill(&self, _skill_name: String) -> PyResult<()> {
        // TODO: Implement
        Ok(())
    }

    /// Remove skill
    fn remove_skill(&self, _skill_name: String) -> PyResult<()> {
        // TODO: Implement
        Ok(())
    }

    /// Compact context
    fn compact(&self) -> PyResult<()> {
        // TODO: Implement
        Ok(())
    }

    /// Clear context
    fn clear(&self) -> PyResult<()> {
        // TODO: Implement
        Ok(())
    }

    /// Configure session
    fn configure(&self, _thinking: Option<bool>, _budget: Option<i32>, _model: Option<ModelConfig>) -> PyResult<()> {
        // TODO: Implement
        Ok(())
    }

    /// Get context usage
    fn context_usage(&self) -> PyResult<PyObject> {
        // TODO: Implement
        Python::with_gil(|py| Ok(PyDict::new(py).into()))
    }

    /// Get history
    fn history(&self) -> PyResult<Vec<PyObject>> {
        // TODO: Implement
        Ok(vec![])
    }

    /// Destroy session
    fn destroy(&self) -> PyResult<()> {
        // TODO: Implement
        Ok(())
    }
}

/// Generate result
#[pyclass]
#[derive(Clone)]
struct GenerateResult {
    #[pyo3(get)]
    text: String,

    #[pyo3(get)]
    usage: TokenUsage,
}

/// Token usage
#[pyclass]
#[derive(Clone)]
struct TokenUsage {
    #[pyo3(get)]
    prompt_tokens: usize,

    #[pyo3(get)]
    completion_tokens: usize,

    #[pyo3(get)]
    total_tokens: usize,
}

/// Create a box (convenience function)
#[pyfunction]
#[pyo3(signature = (workspace=None, skills=None, model=None, resources=None, lanes=None, log_level=None, debug_grpc=false))]
fn create_box(
    workspace: Option<String>,
    skills: Option<Vec<String>>,
    model: Option<ModelConfig>,
    resources: Option<ResourceConfig>,
    lanes: Option<&PyDict>,
    log_level: Option<String>,
    debug_grpc: bool,
) -> PyResult<Box> {
    Box::new(workspace, skills, model, resources, lanes, log_level, debug_grpc)
}
