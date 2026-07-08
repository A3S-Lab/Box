/// Result type used by the direct SDK client.
pub type Result<T> = std::result::Result<T, ClientError>;

/// Errors returned by the direct SDK client.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("state error: {0}")]
    State(#[from] std::io::Error),
    #[error("runtime error: {0}")]
    Runtime(#[from] a3s_box_core::error::BoxError),
    #[error("validation error: {0}")]
    Validation(String),
    #[error("box not found: {0}")]
    BoxNotFound(String),
    #[error("box query {query:?} matched multiple boxes: {matches:?}")]
    AmbiguousBoxQuery { query: String, matches: Vec<String> },
}

/// Filesystem locations used by [`A3sBoxClient`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A3sBoxPaths {
    pub home: PathBuf,
    pub boxes_file: PathBuf,
    pub images_dir: PathBuf,
    pub volumes_file: PathBuf,
    pub volumes_dir: PathBuf,
    pub networks_file: PathBuf,
    pub snapshots_dir: PathBuf,
}

impl A3sBoxPaths {
    /// Build paths under an a3s-box home directory.
    pub fn from_home(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self {
            boxes_file: home.join("boxes.json"),
            images_dir: home.join("images"),
            volumes_file: home.join("volumes.json"),
            volumes_dir: home.join("volumes"),
            networks_file: home.join("networks.json"),
            snapshots_dir: home.join("snapshots"),
            home,
        }
    }
}

impl Default for A3sBoxPaths {
    fn default() -> Self {
        Self::from_home(a3s_box_core::dirs_home())
    }
}

/// Options for listing boxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListBoxesOptions {
    /// Include stopped, dead, and created boxes.
    pub all: bool,
}

impl ListBoxesOptions {
    pub const fn all() -> Self {
        Self { all: true }
    }

    pub const fn active() -> Self {
        Self { all: false }
    }
}

impl Default for ListBoxesOptions {
    fn default() -> Self {
        Self::all()
    }
}

/// Options for reading a bounded box log snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadBoxLogsOptions {
    /// Number of lines to return from the end of the log source.
    pub tail: usize,
}

impl ReadBoxLogsOptions {
    pub const fn tail(tail: usize) -> Self {
        Self { tail }
    }
}

impl Default for ReadBoxLogsOptions {
    fn default() -> Self {
        Self { tail: 100 }
    }
}

/// Optional registry credentials for pull and push operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryCredentials {
    pub username: String,
    pub password: String,
}

impl RegistryCredentials {
    pub fn basic(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }

    fn into_auth(self) -> RegistryAuth {
        RegistryAuth::basic(self.username, self.password)
    }
}

/// Request to pull an OCI image through the runtime image puller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullImage {
    pub reference: String,
    pub force: bool,
    pub platform: Option<String>,
    pub signature_policy: SignaturePolicy,
    pub credentials: Option<RegistryCredentials>,
}

impl PullImage {
    pub fn new(reference: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            force: false,
            platform: None,
            signature_policy: SignaturePolicy::default(),
            credentials: None,
        }
    }

    pub fn force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    pub fn platform(mut self, platform: impl Into<String>) -> Self {
        self.platform = Some(platform.into());
        self
    }

    pub fn signature_policy(mut self, policy: SignaturePolicy) -> Self {
        self.signature_policy = policy;
        self
    }

    pub fn credentials(mut self, credentials: RegistryCredentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    fn validate(&self) -> Result<()> {
        ImageReference::parse(&self.reference).map_err(ClientError::Runtime)?;
        Ok(())
    }

    fn registry_auth(&self) -> Result<RegistryAuth> {
        match self.credentials.clone() {
            Some(credentials) => Ok(credentials.into_auth()),
            None => {
                let parsed =
                    ImageReference::parse(&self.reference).map_err(ClientError::Runtime)?;
                Ok(RegistryAuth::from_credential_store(&parsed.registry))
            }
        }
    }
}

/// Request to build an OCI image through the runtime Dockerfile build engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildImage {
    pub context_dir: PathBuf,
    pub dockerfile_path: PathBuf,
    pub tag: Option<String>,
    pub build_args: HashMap<String, String>,
    pub quiet: bool,
    pub platforms: Vec<Platform>,
    pub target: Option<String>,
    pub no_cache: bool,
}

impl BuildImage {
    pub fn new(context_dir: impl Into<PathBuf>) -> Self {
        let context_dir = context_dir.into();
        Self {
            dockerfile_path: context_dir.join("Dockerfile"),
            context_dir,
            tag: None,
            build_args: HashMap::new(),
            quiet: false,
            platforms: Vec::new(),
            target: None,
            no_cache: false,
        }
    }

    pub fn dockerfile_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.dockerfile_path = path.into();
        self
    }

    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    pub fn build_arg(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.build_args.insert(key.into(), value.into());
        self
    }

    pub fn quiet(mut self, quiet: bool) -> Self {
        self.quiet = quiet;
        self
    }

    pub fn platform(mut self, platform: Platform) -> Self {
        self.platforms.push(platform);
        self
    }

    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    pub fn no_cache(mut self, no_cache: bool) -> Self {
        self.no_cache = no_cache;
        self
    }

    fn validate(&self) -> Result<()> {
        if !self.context_dir.exists() {
            return Err(ClientError::Validation(format!(
                "build context does not exist: {}",
                self.context_dir.display()
            )));
        }
        if !self.dockerfile_path.exists() {
            return Err(ClientError::Validation(format!(
                "Dockerfile does not exist: {}",
                self.dockerfile_path.display()
            )));
        }
        Ok(())
    }
}

/// Result of an image build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildImageSummary {
    pub reference: String,
    pub digest: String,
    pub size_bytes: u64,
    pub layer_count: usize,
}

impl From<RuntimeBuildResult> for BuildImageSummary {
    fn from(result: RuntimeBuildResult) -> Self {
        Self {
            reference: result.reference,
            digest: result.digest,
            size_bytes: result.size,
            layer_count: result.layer_count,
        }
    }
}

/// Request to push a locally cached image through the runtime registry pusher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushImage {
    pub source: String,
    pub target: String,
    pub credentials: Option<RegistryCredentials>,
    pub registry_protocol: RegistryProtocol,
}

impl PushImage {
    pub fn new(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            credentials: None,
            registry_protocol: RegistryProtocol::from_env(),
        }
    }

    pub fn credentials(mut self, credentials: RegistryCredentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    pub fn registry_protocol(mut self, protocol: RegistryProtocol) -> Self {
        self.registry_protocol = protocol;
        self
    }

    pub fn plain_http(mut self, enabled: bool) -> Self {
        if enabled {
            self.registry_protocol = RegistryProtocol::Http;
        }
        self
    }

    fn validate(&self) -> Result<()> {
        if self.source.trim().is_empty() {
            return Err(ClientError::Validation(
                "source image reference cannot be empty".to_string(),
            ));
        }
        ImageReference::parse(&self.target).map_err(ClientError::Runtime)?;
        Ok(())
    }

    fn registry_auth(&self, target: &ImageReference) -> RegistryAuth {
        match self.credentials.clone() {
            Some(credentials) => credentials.into_auth(),
            None => RegistryAuth::from_credential_store(&target.registry),
        }
    }
}

/// Result of an image push.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushImageSummary {
    pub reference: String,
    pub manifest_digest: String,
    pub config_url: String,
    pub manifest_url: String,
}

impl PushImageSummary {
    fn from_push_result(reference: String, result: PushResult) -> Self {
        Self {
            reference,
            manifest_digest: result.manifest_digest,
            config_url: result.config_url,
            manifest_url: result.manifest_url,
        }
    }
}

/// Request to add a tag to a cached image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagImage {
    pub source: String,
    pub target: String,
}

impl TagImage {
    pub fn new(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.source.trim().is_empty() {
            return Err(ClientError::Validation(
                "source image reference cannot be empty".to_string(),
            ));
        }
        validate_tag_target(&self.target)
    }
}

/// Options for stopping a running or paused box.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopBox {
    pub timeout_secs: Option<u64>,
}

impl StopBox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = Some(timeout_secs);
        self
    }
}

/// Options for removing a box.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveBox {
    pub force: bool,
}

impl RemoveBox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }
}

/// Result of removing a box.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveBoxSummary {
    pub id: String,
    pub name: String,
}

/// Request to create a snapshot from a box.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSnapshot {
    pub name: Option<String>,
    pub description: Option<String>,
}

impl CreateSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    fn validate(&self) -> Result<()> {
        if let Some(name) = &self.name {
            validate_name("snapshot", name)?;
        }
        Ok(())
    }
}

/// Request to restore a snapshot into a new box record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreSnapshot {
    pub name: Option<String>,
}

impl RestoreSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    fn validate(&self) -> Result<()> {
        if let Some(name) = &self.name {
            validate_name("box", name)?;
        }
        Ok(())
    }
}

/// How a stop request completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopOutcome {
    AlreadyExited,
    GracefulExit,
    ForceKilled,
}

impl StopOutcome {
    fn inferred_exit_code(self, stop_signal: i32) -> Option<i32> {
        match self {
            Self::AlreadyExited => None,
            Self::GracefulExit => Some(128 + stop_signal),
            Self::ForceKilled => Some(137),
        }
    }
}

/// Result of stopping a box.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopBoxSummary {
    pub id: String,
    pub name: String,
    pub outcome: StopOutcome,
    pub exit_code: Option<i32>,
    pub auto_removed: bool,
    pub box_summary: Option<BoxSummary>,
}
