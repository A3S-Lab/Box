//! BuildKit-in-A3S-VM delegation for `a3s-box build`.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use tokio::process::Command;

const DEFAULT_BUILDKIT_IMAGE: &str = "moby/buildkit:latest";
const DEFAULT_BUILDKIT_CPUS: &str = "4";
const DEFAULT_BUILDKIT_MEMORY: &str = "8g";
const OUTPUT_TAR: &str = "image.tar";
const BUILD_SCRIPT: &str = "a3s-buildkit-build.sh";
const DOCKER_CONFIG_GUEST_PATH: &str = "/root/.docker/config.json";
const BUILDKIT_STATE_DIR: &str = "/var/lib/buildkit";

pub(super) struct Build {
    pub(super) context_dir: PathBuf,
    pub(super) dockerfile_path: PathBuf,
    pub(super) tag: Option<String>,
    pub(super) build_args: Vec<String>,
    pub(super) quiet: bool,
    pub(super) platform: Option<String>,
    pub(super) target: Option<String>,
    pub(super) no_cache: bool,
    pub(super) push: bool,
    pub(super) plain_http: bool,
    pub(super) image: String,
    pub(super) cpus: String,
    pub(super) memory: String,
}

struct BuildkitAuthConfig {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl BuildkitAuthConfig {
    fn path(&self) -> &Path {
        &self.path
    }
}

pub(super) async fn execute(options: Build) -> Result<(), Box<dyn std::error::Error>> {
    if options.push && options.tag.is_none() {
        return Err("--push requires --tag so BuildKit knows which image reference to push".into());
    }

    let dockerfile_arg = dockerfile_arg(&options.context_dir, &options.dockerfile_path)?;
    let output_dir =
        tempfile::tempdir().map_err(|e| format!("Failed to create BuildKit output dir: {e}"))?;
    let output_tar = output_dir.path().join(OUTPUT_TAR);
    let auth_config = if options.push {
        let tag = options
            .tag
            .as_deref()
            .ok_or("--push requires --tag so BuildKit knows which image reference to push")?;
        buildkit_auth_config(tag)?
    } else {
        None
    };

    let buildctl_args = buildctl_args(&options, &dockerfile_arg, &output_tar)?;
    write_build_script(output_dir.path(), &buildctl_args)?;

    let run_args = run_args(
        &options,
        output_dir.path(),
        auth_config.as_ref().map(BuildkitAuthConfig::path),
    )?;
    if !options.quiet {
        eprintln!("Building with BuildKit inside an A3S VM...");
    }
    run_current_a3s_box(&run_args).await?;

    if options.push {
        return Ok(());
    }

    if !output_tar.exists() {
        return Err(format!(
            "BuildKit did not produce the expected OCI archive at {}",
            output_tar.display()
        )
        .into());
    }
    let load_args = load_args(&output_tar, options.tag.as_deref());
    run_current_a3s_box(&load_args).await?;
    Ok(())
}

async fn run_current_a3s_box(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let exe =
        std::env::current_exe().map_err(|e| format!("Failed to locate a3s-box binary: {e}"))?;
    let status = Command::new(&exe)
        .args(args)
        .status()
        .await
        .map_err(|e| format!("Failed to run {} {}: {e}", exe.display(), args.join(" ")))?;
    if !status.success() {
        return Err(format!(
            "`{} {}` failed with status {}",
            exe.display(),
            args.join(" "),
            status
        )
        .into());
    }
    Ok(())
}

fn dockerfile_arg(
    context_dir: &Path,
    dockerfile_path: &Path,
) -> Result<String, Box<dyn std::error::Error>> {
    let context = context_dir.canonicalize().map_err(|e| {
        format!(
            "Failed to canonicalize build context {}: {}",
            context_dir.display(),
            e
        )
    })?;
    let dockerfile = dockerfile_path.canonicalize().map_err(|e| {
        format!(
            "Failed to canonicalize build file {}: {}",
            dockerfile_path.display(),
            e
        )
    })?;
    let rel = dockerfile.strip_prefix(&context).map_err(|_| {
        format!(
            "BuildKit VM delegation requires the build file to be inside the build context: {} is outside {}",
            dockerfile.display(),
            context.display()
        )
    })?;
    let rel = rel.to_str().ok_or_else(|| {
        format!(
            "Build file path is not valid UTF-8 for BuildKit VM delegation: {}",
            rel.display()
        )
    })?;
    Ok(rel.replace('\\', "/"))
}

fn run_args(
    options: &Build,
    output_dir: &Path,
    auth_config: Option<&Path>,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let context = options.context_dir.to_str().ok_or_else(|| {
        format!(
            "Build context path is not valid UTF-8: {}",
            options.context_dir.display()
        )
    })?;
    let output = output_dir.to_str().ok_or_else(|| {
        format!(
            "BuildKit output path is not valid UTF-8: {}",
            output_dir.display()
        )
    })?;
    let mut args = vec![
        "run".to_string(),
        "--rm".to_string(),
        "--no-stdin".to_string(),
        "--cpus".to_string(),
        options.cpus.clone(),
        "--memory".to_string(),
        options.memory.clone(),
        "--privileged".to_string(),
        "--volume".to_string(),
        format!("{context}:/workspace:ro"),
        "--volume".to_string(),
        format!("{output}:/out"),
        "--tmpfs".to_string(),
        BUILDKIT_STATE_DIR.to_string(),
    ];

    if let Some(auth_config) = auth_config {
        let auth_config = auth_config.to_str().ok_or_else(|| {
            format!(
                "BuildKit auth config path is not valid UTF-8: {}",
                auth_config.display()
            )
        })?;
        args.push("--volume".to_string());
        args.push(format!("{auth_config}:{DOCKER_CONFIG_GUEST_PATH}:ro"));
    }

    args.extend([
        "--entrypoint".to_string(),
        "/bin/sh".to_string(),
        options.image.clone(),
        "--".to_string(),
        format!("/out/{BUILD_SCRIPT}"),
    ]);
    Ok(args)
}

fn buildctl_args(
    options: &Build,
    dockerfile_arg: &str,
    output_tar: &Path,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let output_name = output_tar
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("Invalid BuildKit output file: {}", output_tar.display()))?;
    let mut args = vec![
        "build".to_string(),
        "--frontend".to_string(),
        "dockerfile.v0".to_string(),
        "--local".to_string(),
        "context=/workspace".to_string(),
        "--local".to_string(),
        "dockerfile=/workspace".to_string(),
        "--opt".to_string(),
        format!("filename={dockerfile_arg}"),
    ];

    for build_arg in &options.build_args {
        args.push("--opt".to_string());
        args.push(format!("build-arg:{build_arg}"));
    }
    if let Some(platform) = &options.platform {
        args.push("--opt".to_string());
        args.push(format!("platform={platform}"));
    }
    if let Some(target) = &options.target {
        args.push("--opt".to_string());
        args.push(format!("target={target}"));
    }
    if options.no_cache {
        args.push("--no-cache".to_string());
    }

    args.push("--output".to_string());
    args.push(output_attr(options, output_name)?);
    Ok(args)
}

fn write_build_script(
    output_dir: &Path,
    args: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let script_path = output_dir.join(BUILD_SCRIPT);
    let mut script = String::from("#!/bin/sh\nset -eu\nexec buildctl-daemonless.sh");
    for arg in args {
        script.push(' ');
        script.push_str(&shell_quote(arg));
    }
    script.push('\n');
    std::fs::write(&script_path, script).map_err(|error| {
        format!(
            "Failed to write BuildKit helper script {}: {error}",
            script_path.display()
        )
    })?;
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn buildkit_auth_config(
    tag: &str,
) -> Result<Option<BuildkitAuthConfig>, Box<dyn std::error::Error>> {
    let reference = a3s_box_runtime::ImageReference::parse(tag)?;
    let auth = a3s_box_runtime::RegistryAuth::from_credential_store(&reference.registry);
    let Some((username, password)) = auth.basic_credentials() else {
        return Ok(None);
    };

    Ok(Some(write_buildkit_auth_config(
        &reference.registry,
        &username,
        &password,
    )?))
}

fn write_buildkit_auth_config(
    registry: &str,
    username: &str,
    password: &str,
) -> Result<BuildkitAuthConfig, Box<dyn std::error::Error>> {
    let dir =
        tempfile::tempdir().map_err(|e| format!("Failed to create BuildKit auth dir: {e}"))?;
    let path = dir.path().join("config.json");
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    let mut auths = serde_json::Map::new();
    for key in docker_config_registry_keys(registry) {
        auths.insert(key, serde_json::json!({ "auth": auth.clone() }));
    }
    let config = serde_json::json!({ "auths": auths });
    let data = serde_json::to_vec_pretty(&config)?;
    std::fs::write(&path, data).map_err(|e| {
        format!(
            "Failed to write BuildKit Docker auth config {}: {}",
            path.display(),
            e
        )
    })?;

    Ok(BuildkitAuthConfig { _dir: dir, path })
}

fn docker_config_registry_keys(registry: &str) -> Vec<String> {
    let registry = registry.trim().to_lowercase();
    let mut keys = if matches!(
        registry.as_str(),
        "docker.io" | "index.docker.io" | "registry-1.docker.io"
    ) {
        vec![
            "docker.io".to_string(),
            "index.docker.io".to_string(),
            "registry-1.docker.io".to_string(),
            "https://index.docker.io/v1/".to_string(),
        ]
    } else {
        vec![registry]
    };
    keys.sort();
    keys.dedup();
    keys
}

fn output_attr(options: &Build, output_name: &str) -> Result<String, Box<dyn std::error::Error>> {
    if options.push {
        let tag = options
            .tag
            .as_deref()
            .ok_or("--push requires --tag so BuildKit knows which image reference to push")?;
        let insecure = if options.plain_http {
            ",registry.insecure=true"
        } else {
            ""
        };
        return Ok(format!("type=image,name={tag},push=true{insecure}"));
    }

    Ok(format!("type=oci,dest=/out/{output_name}"))
}

fn load_args(output_tar: &Path, tag: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "load".to_string(),
        "--input".to_string(),
        output_tar.to_string_lossy().to_string(),
    ];
    if let Some(tag) = tag {
        args.push("--tag".to_string());
        args.push(tag.to_string());
    }
    args
}

pub(super) fn default_image() -> String {
    std::env::var("A3S_BOX_BUILDKIT_IMAGE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BUILDKIT_IMAGE.to_string())
}

pub(super) fn default_cpus() -> String {
    std::env::var("A3S_BOX_BUILDKIT_CPUS")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BUILDKIT_CPUS.to_string())
}

pub(super) fn default_memory() -> String {
    std::env::var("A3S_BOX_BUILDKIT_MEMORY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BUILDKIT_MEMORY.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_options() -> Build {
        Build {
            context_dir: PathBuf::from("/context"),
            dockerfile_path: PathBuf::from("/context/docker/Dockerfile.web"),
            tag: Some("example.com/app:latest".to_string()),
            build_args: vec!["VERSION=1.2.3".to_string()],
            quiet: true,
            platform: Some("linux/arm64".to_string()),
            target: Some("builder".to_string()),
            no_cache: true,
            push: false,
            plain_http: false,
            image: "moby/buildkit:latest".to_string(),
            cpus: "6".to_string(),
            memory: "8g".to_string(),
        }
    }

    #[test]
    fn test_dockerfile_arg_requires_file_inside_context() {
        let tmp = tempfile::tempdir().unwrap();
        let context = tmp.path().join("context");
        let outside = tmp.path().join("Dockerfile.outside");
        std::fs::create_dir_all(context.join("docker")).unwrap();
        std::fs::write(context.join("docker/Dockerfile.web"), "FROM scratch\n").unwrap();
        std::fs::write(&outside, "FROM scratch\n").unwrap();

        let rel = dockerfile_arg(&context, &context.join("docker/Dockerfile.web")).unwrap();
        assert_eq!(rel, "docker/Dockerfile.web");

        let err = dockerfile_arg(&context, &outside).unwrap_err().to_string();
        assert!(err.contains("inside the build context"));
    }

    #[test]
    fn test_run_args_buildkit_daemonless_oci_output() {
        let options = base_options();
        let args = run_args(&options, Path::new("/tmp/out"), None).unwrap();
        let build_args = buildctl_args(
            &options,
            "docker/Dockerfile.web",
            Path::new("/tmp/out/image.tar"),
        )
        .unwrap();

        assert_eq!(args[0], "run");
        assert!(args.contains(&"--privileged".to_string()));
        assert!(args.contains(&"moby/buildkit:latest".to_string()));
        assert!(args.contains(&"/bin/sh".to_string()));
        assert!(args.contains(&format!("/out/{BUILD_SCRIPT}")));
        assert!(args.contains(&"--tmpfs".to_string()));
        assert!(args.contains(&"/var/lib/buildkit".to_string()));
        assert!(build_args.contains(&"context=/workspace".to_string()));
        assert!(build_args.contains(&"dockerfile=/workspace".to_string()));
        assert!(build_args.contains(&"filename=docker/Dockerfile.web".to_string()));
        assert!(build_args.contains(&"build-arg:VERSION=1.2.3".to_string()));
        assert!(build_args.contains(&"platform=linux/arm64".to_string()));
        assert!(build_args.contains(&"target=builder".to_string()));
        assert!(build_args.contains(&"--no-cache".to_string()));
        assert!(build_args.contains(&"type=oci,dest=/out/image.tar".to_string()));
    }

    #[test]
    fn test_run_args_buildkit_image_push_output() {
        let mut options = base_options();
        options.push = true;
        options.plain_http = true;
        let args = buildctl_args(
            &options,
            "docker/Dockerfile.web",
            Path::new("/tmp/out/image.tar"),
        )
        .unwrap();

        assert!(args.contains(
            &"type=image,name=example.com/app:latest,push=true,registry.insecure=true".to_string()
        ));
        assert!(!args.contains(&"type=oci,dest=/out/image.tar".to_string()));
    }

    #[test]
    fn test_run_args_mounts_buildkit_auth_config() {
        let options = base_options();
        let args = run_args(
            &options,
            Path::new("/tmp/out"),
            Some(Path::new("/tmp/auth/config.json")),
        )
        .unwrap();

        assert!(args.contains(&"/tmp/auth/config.json:/root/.docker/config.json:ro".to_string()));
    }

    #[test]
    fn test_build_script_preserves_spaces_quotes_and_multiple_build_args() {
        let tmp = tempfile::tempdir().unwrap();
        let args = vec![
            "build".to_string(),
            "--opt".to_string(),
            "build-arg:PLAIN=custom".to_string(),
            "--opt".to_string(),
            "build-arg:QUOTED=two words and 'quote'".to_string(),
        ];

        write_build_script(tmp.path(), &args).unwrap();
        let script = std::fs::read_to_string(tmp.path().join(BUILD_SCRIPT)).unwrap();

        assert!(script.starts_with("#!/bin/sh\nset -eu\nexec buildctl-daemonless.sh "));
        assert!(script.contains("'build-arg:PLAIN=custom'"));
        assert!(script.contains("'build-arg:QUOTED=two words and '\"'\"'quote'\"'\"''"));
    }

    #[test]
    fn test_write_buildkit_auth_config_writes_single_registry_auth() {
        let config = write_buildkit_auth_config("ghcr.io", "user", "secret").unwrap();
        let data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(config.path()).unwrap()).unwrap();

        let auth = data["auths"]["ghcr.io"]["auth"].as_str().unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(auth)
            .unwrap();

        assert_eq!(decoded, b"user:secret");
    }

    #[test]
    fn test_docker_config_registry_keys_include_docker_hub_aliases() {
        assert_eq!(
            docker_config_registry_keys("docker.io"),
            vec![
                "docker.io".to_string(),
                "https://index.docker.io/v1/".to_string(),
                "index.docker.io".to_string(),
                "registry-1.docker.io".to_string(),
            ]
        );
    }

    #[test]
    fn test_output_attr_push_requires_tag() {
        let mut options = base_options();
        options.push = true;
        options.tag = None;

        let err = output_attr(&options, "image.tar").unwrap_err().to_string();

        assert!(err.contains("--push requires --tag"));
    }

    #[test]
    fn test_load_args_adds_tag() {
        let args = load_args(Path::new("/tmp/out/image.tar"), Some("app:latest"));
        assert_eq!(
            args,
            vec![
                "load",
                "--input",
                "/tmp/out/image.tar",
                "--tag",
                "app:latest"
            ]
        );
    }

    #[test]
    fn test_run_args_accepts_amd64_platform_for_buildkit_emulation() {
        let mut options = base_options();
        options.platform = Some("linux/amd64".to_string());

        let args = buildctl_args(
            &options,
            "docker/Dockerfile.web",
            Path::new("/tmp/out/image.tar"),
        )
        .unwrap();

        assert!(args.contains(&"platform=linux/amd64".to_string()));
    }
}
