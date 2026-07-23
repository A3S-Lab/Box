/// Runtime-backed SDK client for local a3s-box state.
#[derive(Clone)]
pub struct A3sBoxClient {
    paths: A3sBoxPaths,
    image_cache_size: u64,
    execution_manager: Arc<dyn ExecutionManager>,
    execution_session_manager: Option<Arc<dyn ExecutionSessionManager>>,
}

impl std::fmt::Debug for A3sBoxClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("A3sBoxClient")
            .field("paths", &self.paths)
            .field("image_cache_size", &self.image_cache_size)
            .finish_non_exhaustive()
    }
}

impl A3sBoxClient {
    /// Create a client for the default a3s-box home.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a client rooted at a custom a3s-box home directory.
    pub fn from_home(home: impl Into<PathBuf>) -> Self {
        Self::with_paths(A3sBoxPaths::from_home(home))
    }

    /// Create a client using explicit state paths.
    pub fn with_paths(paths: A3sBoxPaths) -> Self {
        let local_execution_manager = Arc::new(
            a3s_box_runtime::LocalExecutionManager::with_vm_backend(
                paths.boxes_file.clone(),
                paths.home.clone(),
            ),
        );
        Self {
            paths,
            image_cache_size: a3s_box_runtime::DEFAULT_IMAGE_CACHE_SIZE,
            execution_manager: local_execution_manager.clone(),
            execution_session_manager: Some(local_execution_manager),
        }
    }

    /// Create a client with an explicit backend-neutral execution manager.
    ///
    /// This keeps lifecycle calls on the same canonical facade used by the CLI
    /// and remote compatibility service while allowing an embedding application
    /// to inject its own manager implementation.
    pub fn with_execution_manager(
        paths: A3sBoxPaths,
        execution_manager: Arc<dyn ExecutionManager>,
    ) -> Self {
        Self {
            paths,
            image_cache_size: a3s_box_runtime::DEFAULT_IMAGE_CACHE_SIZE,
            execution_manager,
            execution_session_manager: None,
        }
    }

    /// Create a client with explicit typed lifecycle and session managers.
    ///
    /// This is useful for embedding the E2B-style [`crate::Sandbox`] facade in
    /// another local process without selecting backends by string.
    pub fn with_execution_services(
        paths: A3sBoxPaths,
        execution_manager: Arc<dyn ExecutionManager>,
        execution_session_manager: Arc<dyn ExecutionSessionManager>,
    ) -> Self {
        Self {
            paths,
            image_cache_size: a3s_box_runtime::DEFAULT_IMAGE_CACHE_SIZE,
            execution_manager,
            execution_session_manager: Some(execution_session_manager),
        }
    }

    /// Override the image cache size used when opening the runtime image store.
    pub fn with_image_cache_size(mut self, image_cache_size: u64) -> Self {
        self.image_cache_size = image_cache_size;
        self
    }

    /// Return the state paths used by this client.
    pub fn paths(&self) -> &A3sBoxPaths {
        &self.paths
    }

    /// Collect local runtime diagnostics without spawning the CLI.
    pub fn runtime_diagnostics(&self) -> RuntimeDiagnostics {
        RuntimeDiagnostics::collect(&self.paths)
    }

    /// Collect local runtime disk usage without spawning the CLI.
    pub fn runtime_disk_usage(&self) -> Result<RuntimeDiskUsage> {
        RuntimeDiskUsage::collect(&self.paths)
    }
}

impl Default for A3sBoxClient {
    fn default() -> Self {
        Self::with_paths(A3sBoxPaths::default())
    }
}
