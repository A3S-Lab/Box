/// Fluent OCI image builder backed by the local runtime build engine.
#[derive(Debug, Clone)]
pub struct ImageBuilder {
    client: A3sBoxClient,
    request: BuildImage,
}

impl ImageBuilder {
    fn new(client: A3sBoxClient, context_dir: impl Into<PathBuf>) -> Self {
        Self {
            client,
            request: BuildImage::new(context_dir),
        }
    }

    /// Select a Dockerfile. Relative paths are resolved from the build context.
    pub fn dockerfile(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        self.request.dockerfile_path = if path.is_relative() {
            self.request.context_dir.join(path)
        } else {
            path
        };
        self
    }

    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.request.tag = Some(tag.into());
        self
    }

    pub fn build_arg(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.request.build_args.insert(key.into(), value.into());
        self
    }

    pub const fn quiet(mut self, quiet: bool) -> Self {
        self.request.quiet = quiet;
        self
    }

    pub fn platform(mut self, platform: Platform) -> Self {
        self.request.platforms.push(platform);
        self
    }

    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.request.target = Some(target.into());
        self
    }

    pub const fn no_cache(mut self, no_cache: bool) -> Self {
        self.request.no_cache = no_cache;
        self
    }

    pub fn request(&self) -> &BuildImage {
        &self.request
    }

    pub async fn build(self) -> Result<BuildImageSummary> {
        self.client.build_image(self.request).await
    }
}

/// Fluent named-volume builder.
#[derive(Debug, Clone)]
pub struct VolumeBuilder {
    client: A3sBoxClient,
    request: CreateVolume,
}

impl VolumeBuilder {
    fn new(client: A3sBoxClient, name: impl Into<String>) -> Self {
        Self {
            client,
            request: CreateVolume::new(name),
        }
    }

    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.request.labels.insert(key.into(), value.into());
        self
    }

    pub const fn size_limit(mut self, bytes: u64) -> Self {
        self.request.size_limit = bytes;
        self
    }

    pub fn request(&self) -> &CreateVolume {
        &self.request
    }

    pub fn create(self) -> Result<VolumeSummary> {
        self.client.create_volume(self.request)
    }
}

/// Fluent bridge-network builder.
#[derive(Debug, Clone)]
pub struct NetworkBuilder {
    client: A3sBoxClient,
    request: CreateNetwork,
}

impl NetworkBuilder {
    fn new(client: A3sBoxClient, name: impl Into<String>) -> Self {
        Self {
            client,
            request: CreateNetwork::new(name),
        }
    }

    pub fn subnet(mut self, subnet: impl Into<String>) -> Self {
        self.request.subnet = subnet.into();
        self
    }

    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.request.labels.insert(key.into(), value.into());
        self
    }

    pub const fn isolation(mut self, isolation: IsolationMode) -> Self {
        self.request.isolation = isolation;
        self
    }

    pub fn request(&self) -> &CreateNetwork {
        &self.request
    }

    pub fn create(self) -> Result<NetworkSummary> {
        self.client.create_network(self.request)
    }
}

impl A3sBoxClient {
    /// Start a fluent local OCI image build.
    pub fn image(&self, context_dir: impl Into<PathBuf>) -> ImageBuilder {
        ImageBuilder::new(self.clone(), context_dir)
    }

    /// Start a fluent named-volume creation request.
    pub fn volume(&self, name: impl Into<String>) -> VolumeBuilder {
        VolumeBuilder::new(self.clone(), name)
    }

    /// Start a fluent bridge-network creation request.
    pub fn network(&self, name: impl Into<String>) -> NetworkBuilder {
        NetworkBuilder::new(self.clone(), name)
    }
}
