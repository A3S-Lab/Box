//! Tests for the build engine.

#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use super::super::utils::*;
    use super::super::{
        build, default_target_platform, scratch_config, validate_build_config, BuildConfig,
        BuildState,
    };
    use crate::oci::{ImageStore, OciImage};
    use a3s_box_core::platform::Platform;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    #[test]
    fn test_resolve_path_absolute() {
        assert_eq!(resolve_path("/app", "/usr/bin"), "/usr/bin");
    }

    #[test]
    fn test_resolve_path_relative() {
        assert_eq!(resolve_path("/app", "src"), "/app/src");
    }

    #[test]
    fn test_resolve_path_root_workdir() {
        assert_eq!(resolve_path("/", "app"), "/app");
    }

    #[test]
    fn test_expand_args_braces() {
        let mut args = HashMap::new();
        args.insert("VERSION".to_string(), "3.19".to_string());
        assert_eq!(expand_args("alpine:${VERSION}", &args), "alpine:3.19");
    }

    #[test]
    fn test_expand_args_dollar() {
        let mut args = HashMap::new();
        args.insert("TAG".to_string(), "latest".to_string());
        assert_eq!(expand_args("image:$TAG", &args), "image:latest");
    }

    #[test]
    fn test_expand_args_no_match() {
        let args = HashMap::new();
        assert_eq!(expand_args("alpine:3.19", &args), "alpine:3.19");
    }

    #[test]
    fn test_run_env_includes_declared_args_and_env_overrides() {
        let mut build_args = HashMap::new();
        build_args.insert(
            "ALPINE_MIRROR".to_string(),
            "mirrors.tencent.com".to_string(),
        );
        build_args.insert("MODE".to_string(), "prod".to_string());
        build_args.insert("UNDECLARED".to_string(), "ignored".to_string());

        let mut state = BuildState::new(build_args);
        state.declared_args.insert("ALPINE_MIRROR".to_string());
        state.declared_args.insert("MODE".to_string());
        state.env.push(("MODE".to_string(), "debug".to_string()));

        let env = state.run_env().into_iter().collect::<HashMap<_, _>>();

        assert_eq!(
            env.get("ALPINE_MIRROR").map(String::as_str),
            Some("mirrors.tencent.com")
        );
        assert_eq!(env.get("MODE").map(String::as_str), Some("debug"));
        assert!(!env.contains_key("UNDECLARED"));
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1_500_000), "1.4 MB");
        assert_eq!(format_size(1_500_000_000), "1.4 GB");
    }

    fn test_build_config(platforms: Vec<Platform>) -> BuildConfig {
        BuildConfig {
            context_dir: PathBuf::from("/tmp/context"),
            dockerfile_path: PathBuf::from("/tmp/context/Dockerfile"),
            tag: Some("test:latest".to_string()),
            build_args: HashMap::new(),
            quiet: true,
            platforms,
            target: None,
            no_cache: false,
            metrics: None,
            run_pool: None,
        }
    }

    #[cfg(all(feature = "pool", not(windows)))]
    fn parse_pool_volume_spec(spec: &str) -> (PathBuf, String, String) {
        let parts = spec.rsplitn(3, ':').collect::<Vec<_>>();
        assert_eq!(parts.len(), 3, "expected host:guest:mode volume spec");
        (
            PathBuf::from(parts[2]),
            parts[1].to_string(),
            parts[0].to_string(),
        )
    }

    #[cfg(all(feature = "pool", not(windows)))]
    fn image_layer_files(image: &OciImage) -> Vec<String> {
        let mut names = Vec::new();
        for layer in image.layer_paths() {
            let file = std::fs::File::open(layer).unwrap();
            let dec = flate2::read::GzDecoder::new(file);
            let mut ar = tar::Archive::new(dec);
            names.extend(
                ar.entries()
                    .unwrap()
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| entry.header().entry_type().is_file())
                    .map(|entry| entry.path().unwrap().to_string_lossy().to_string()),
            );
        }
        names.sort();
        names
    }

    #[cfg(all(feature = "pool", not(windows)))]
    fn tree_contains_file_named(root: &std::path::Path, file_name: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(root) else {
            return false;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.is_file() && path.file_name().and_then(|name| name.to_str()) == Some(file_name)
            {
                return true;
            }
            if path.is_dir() && tree_contains_file_named(&path, file_name) {
                return true;
            }
        }
        false
    }

    #[test]
    fn test_validate_build_config_rejects_multi_platform() {
        let config = test_build_config(vec![Platform::linux_amd64(), Platform::linux_arm64()]);
        let err = validate_build_config(&config).unwrap_err().to_string();
        assert!(err.contains("Multi-platform builds are not implemented yet"));
    }

    #[test]
    fn test_validate_build_config_rejects_non_linux_platform() {
        let config = test_build_config(vec![Platform::new("windows", "amd64")]);
        let err = validate_build_config(&config).unwrap_err().to_string();
        assert!(err.contains("Only linux target platforms"));
    }

    #[test]
    fn test_default_target_platform_is_linux() {
        let platform = default_target_platform();
        assert_eq!(platform.os, "linux");
        assert!(!platform.architecture.is_empty());
    }

    #[test]
    fn test_scratch_config_is_empty_base() {
        let config = scratch_config();
        assert!(config.entrypoint.is_none());
        assert!(config.cmd.is_none());
        assert!(config.env.is_empty());
        assert!(config.volumes.is_empty());
    }

    #[tokio::test]
    async fn test_build_from_scratch_copy_metadata_without_network() {
        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let store_dir = tmp.path().join("images");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("hello.txt"), "hello").unwrap();
        std::fs::write(
            context.join("Dockerfile"),
            r#"FROM scratch
COPY hello.txt /hello.txt
CMD ["cat", "/hello.txt"]
LABEL org.opencontainers.image.title="scratch-smoke"
"#,
        )
        .unwrap();

        let store = Arc::new(ImageStore::new(&store_dir, 1024 * 1024 * 100).unwrap());
        let result = build(
            BuildConfig {
                context_dir: context.clone(),
                dockerfile_path: context.join("Dockerfile"),
                tag: Some("scratch-smoke:latest".to_string()),
                build_args: HashMap::new(),
                quiet: true,
                platforms: vec![],
                target: None,
                no_cache: false,
                metrics: None,
                run_pool: None,
            },
            store.clone(),
        )
        .await
        .unwrap();

        assert_eq!(result.reference, "scratch-smoke:latest");
        assert_eq!(result.layer_count, 1);

        let stored = store.get("scratch-smoke:latest").await.unwrap();
        let image = OciImage::from_path(&stored.path).unwrap();
        assert_eq!(
            image.config().cmd,
            Some(vec!["cat".to_string(), "/hello.txt".to_string()])
        );
        assert_eq!(
            image.label("org.opencontainers.image.title"),
            Some("scratch-smoke")
        );
    }

    #[cfg(all(feature = "pool", not(windows)))]
    #[tokio::test]
    async fn test_build_run_pool_reuses_stage_lease_and_captures_rootfs_diff() {
        use super::super::BuildRunPoolConfig;
        use crate::pool::client::{read_frame, write_frame};
        use crate::pool::{
            PoolLeaseReleaseResponse, PoolLeaseResponse, PoolRequest, PoolRunResponse,
        };
        use std::time::Duration;
        use tokio::net::UnixListener;

        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let store_dir = tmp.path().join("images");
        let socket = tmp.path().join("pool.sock");
        let run_cache_dir = tmp.path().join("run-cache");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("sh"), "fake shell").unwrap();
        std::fs::write(
            context.join("Dockerfile"),
            r#"FROM scratch
COPY sh /bin/sh
WORKDIR /app
ENV HELLO=warm
USER 1000:1001
RUN echo "$HELLO" > out.txt
RUN cat out.txt > copied.txt
RUN ["/bin/sh", "-c", "printf exec > exec.txt"]
CMD ["cat", "/app/copied.txt"]
"#,
        )
        .unwrap();

        let listener = UnixListener::bind(&socket).unwrap();
        let daemon = tokio::spawn(async move {
            let mut lease_count = 0usize;
            let mut exec_count = 0usize;
            let mut release_count = 0usize;
            let mut rootfs_host: Option<PathBuf> = None;

            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let req: PoolRequest =
                    serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();

                match req {
                    PoolRequest::Lease(req) => {
                        lease_count += 1;
                        assert_eq!(lease_count, 1, "stage should lease exactly one VM");
                        assert_eq!(req.image.as_deref(), Some("helper:latest"));
                        assert_eq!(req.vcpus, Some(3));
                        assert_eq!(req.memory_mb, Some(768));
                        assert_eq!(req.volumes.len(), 1);

                        let (host, guest, mode) = parse_pool_volume_spec(&req.volumes[0]);
                        assert_eq!(guest, "/run/a3s/test-rootfs");
                        assert_eq!(mode, "rw");
                        rootfs_host = Some(host);

                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolLeaseResponse {
                                lease_id: Some("lease-1".to_string()),
                                error: None,
                            })
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    PoolRequest::Exec(req) => {
                        exec_count += 1;
                        assert_eq!(req.lease_id, "lease-1");
                        assert_eq!(req.rootfs.as_deref(), Some("/run/a3s/test-rootfs"));
                        assert_eq!(req.timeout_ns, Some(12_000_000_000));
                        assert_eq!(req.user.as_deref(), Some("1000:1001"));
                        assert!(req.env.iter().any(|entry| entry == "HELLO=warm"));

                        let rootfs = rootfs_host.as_ref().expect("lease records rootfs host");
                        match exec_count {
                            1 => {
                                assert_eq!(req.working_dir.as_deref(), Some("/"));
                                assert_eq!(
                                    req.cmd,
                                    vec![
                                        "/bin/sh".to_string(),
                                        "-c".to_string(),
                                        "cd '/app' && echo \"$HELLO\" > out.txt".to_string(),
                                    ]
                                );
                                std::fs::write(rootfs.join("app/out.txt"), "warm\n").unwrap();
                            }
                            2 => {
                                assert_eq!(req.working_dir.as_deref(), Some("/"));
                                assert_eq!(
                                    req.cmd,
                                    vec![
                                        "/bin/sh".to_string(),
                                        "-c".to_string(),
                                        "cd '/app' && cat out.txt > copied.txt".to_string(),
                                    ]
                                );
                                let content = std::fs::read_to_string(rootfs.join("app/out.txt"))
                                    .expect("second RUN sees first RUN output in the same rootfs");
                                std::fs::write(rootfs.join("app/copied.txt"), content).unwrap();
                            }
                            3 => {
                                assert_eq!(req.working_dir.as_deref(), Some("/app"));
                                assert_eq!(
                                    req.cmd,
                                    vec![
                                        "/bin/sh".to_string(),
                                        "-c".to_string(),
                                        "printf exec > exec.txt".to_string(),
                                    ]
                                );
                                std::fs::write(rootfs.join("app/exec.txt"), "exec").unwrap();
                            }
                            other => panic!("unexpected exec request {other}"),
                        }

                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolRunResponse {
                                stdout: Vec::new(),
                                stderr: Vec::new(),
                                exit_code: 0,
                                error: None,
                            })
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    PoolRequest::Release(req) => {
                        release_count += 1;
                        assert_eq!(req.lease_id, "lease-1");
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolLeaseReleaseResponse { error: None }).unwrap(),
                        )
                        .await
                        .unwrap();
                        break;
                    }
                    other => panic!(
                        "unexpected pool request: {:?}",
                        std::mem::discriminant(&other)
                    ),
                }
            }

            assert_eq!(lease_count, 1);
            assert_eq!(exec_count, 3);
            assert_eq!(release_count, 1);
        });

        let store = Arc::new(ImageStore::new(&store_dir, 1024 * 1024 * 100).unwrap());
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            build(
                BuildConfig {
                    context_dir: context.clone(),
                    dockerfile_path: context.join("Dockerfile"),
                    tag: Some("run-pool:latest".to_string()),
                    build_args: HashMap::new(),
                    quiet: true,
                    platforms: vec![],
                    target: None,
                    no_cache: true,
                    metrics: None,
                    run_pool: Some(BuildRunPoolConfig {
                        socket: socket.to_string_lossy().to_string(),
                        image: Some("helper:latest".to_string()),
                        vcpus: 3,
                        memory_mb: 768,
                        guest_rootfs: "/run/a3s/test-rootfs".to_string(),
                        timeout_ns: 12_000_000_000,
                        run_cache_dir: run_cache_dir.clone(),
                    }),
                },
                store.clone(),
            ),
        )
        .await
        .expect("build should not hang")
        .unwrap();

        daemon.await.unwrap();
        assert_eq!(result.layer_count, 4);

        let stored = store.get("run-pool:latest").await.unwrap();
        let image = OciImage::from_path(&stored.path).unwrap();
        let files = image_layer_files(&image);
        assert_eq!(image.config().user.as_deref(), Some("1000:1001"));
        assert!(files.iter().any(|path| path == "bin/sh"));
        assert!(files.iter().any(|path| path == "app/out.txt"));
        assert!(files.iter().any(|path| path == "app/copied.txt"));
        assert!(files.iter().any(|path| path == "app/exec.txt"));
    }

    #[cfg(all(feature = "pool", not(windows)))]
    #[tokio::test]
    async fn test_build_run_pool_bind_mount_context_is_not_committed() {
        use super::super::BuildRunPoolConfig;
        use crate::pool::client::{read_frame, write_frame};
        use crate::pool::{
            PoolLeaseReleaseResponse, PoolLeaseResponse, PoolRequest, PoolRunResponse,
        };
        use std::time::Duration;
        use tokio::net::UnixListener;

        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let store_dir = tmp.path().join("images");
        let socket = tmp.path().join("pool.sock");
        let run_cache_dir = tmp.path().join("run-cache");
        std::fs::create_dir_all(context.join("src")).unwrap();
        std::fs::write(context.join("sh"), "fake shell").unwrap();
        std::fs::write(context.join("src/input.txt"), "from-bind\n").unwrap();
        std::fs::write(
            context.join("Dockerfile"),
            r#"FROM scratch
COPY sh /bin/sh
WORKDIR /work
RUN --mount=type=bind,source=src,target=. cat input.txt > /out.txt
"#,
        )
        .unwrap();

        let listener = UnixListener::bind(&socket).unwrap();
        let daemon = tokio::spawn(async move {
            let mut rootfs_host: Option<PathBuf> = None;

            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let req: PoolRequest =
                    serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();

                match req {
                    PoolRequest::Lease(req) => {
                        let (host, guest, mode) = parse_pool_volume_spec(&req.volumes[0]);
                        assert_eq!(guest, "/run/a3s/test-rootfs");
                        assert_eq!(mode, "rw");
                        rootfs_host = Some(host);
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolLeaseResponse {
                                lease_id: Some("lease-bind".to_string()),
                                error: None,
                            })
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    PoolRequest::Exec(req) => {
                        assert_eq!(req.lease_id, "lease-bind");
                        assert_eq!(
                            req.cmd,
                            vec![
                                "/bin/sh".to_string(),
                                "-c".to_string(),
                                "cd '/work' && cat input.txt > /out.txt".to_string(),
                            ]
                        );
                        let rootfs = rootfs_host.as_ref().expect("lease records rootfs host");
                        assert_eq!(
                            std::fs::read_to_string(rootfs.join("work/input.txt")).unwrap(),
                            "from-bind\n"
                        );
                        std::fs::write(rootfs.join("out.txt"), "from-bind\n").unwrap();
                        std::fs::write(rootfs.join("work/generated.txt"), "discarded\n").unwrap();
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolRunResponse {
                                stdout: Vec::new(),
                                stderr: Vec::new(),
                                exit_code: 0,
                                error: None,
                            })
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    PoolRequest::Release(req) => {
                        assert_eq!(req.lease_id, "lease-bind");
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolLeaseReleaseResponse { error: None }).unwrap(),
                        )
                        .await
                        .unwrap();
                        break;
                    }
                    other => panic!(
                        "unexpected pool request: {:?}",
                        std::mem::discriminant(&other)
                    ),
                }
            }
        });

        let store = Arc::new(ImageStore::new(&store_dir, 1024 * 1024 * 100).unwrap());
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            build(
                BuildConfig {
                    context_dir: context.clone(),
                    dockerfile_path: context.join("Dockerfile"),
                    tag: Some("run-pool-bind:latest".to_string()),
                    build_args: HashMap::new(),
                    quiet: true,
                    platforms: vec![],
                    target: None,
                    no_cache: true,
                    metrics: None,
                    run_pool: Some(BuildRunPoolConfig {
                        socket: socket.to_string_lossy().to_string(),
                        image: Some("helper:latest".to_string()),
                        vcpus: 2,
                        memory_mb: 512,
                        guest_rootfs: "/run/a3s/test-rootfs".to_string(),
                        timeout_ns: 12_000_000_000,
                        run_cache_dir: run_cache_dir.clone(),
                    }),
                },
                store.clone(),
            ),
        )
        .await
        .expect("build should not hang")
        .unwrap();

        daemon.await.unwrap();
        assert_eq!(result.layer_count, 2);

        let stored = store.get("run-pool-bind:latest").await.unwrap();
        let image = OciImage::from_path(&stored.path).unwrap();
        let files = image_layer_files(&image);
        assert!(files.iter().any(|path| path == "out.txt"));
        assert!(!files.iter().any(|path| path == "work/input.txt"));
        assert!(!files.iter().any(|path| path == "work/generated.txt"));
    }

    #[cfg(all(feature = "pool", not(windows)))]
    #[tokio::test]
    async fn test_build_run_pool_bind_mount_from_stage_is_not_committed() {
        use super::super::BuildRunPoolConfig;
        use crate::pool::client::{read_frame, write_frame};
        use crate::pool::{
            PoolLeaseReleaseResponse, PoolLeaseResponse, PoolRequest, PoolRunResponse,
        };
        use std::collections::HashMap;
        use std::time::Duration;
        use tokio::net::UnixListener;

        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let store_dir = tmp.path().join("images");
        let socket = tmp.path().join("pool.sock");
        let run_cache_dir = tmp.path().join("run-cache");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("sh"), "fake shell").unwrap();
        std::fs::write(
            context.join("Dockerfile"),
            r#"FROM scratch AS builder
COPY sh /bin/sh
RUN printf built > /artifact.txt

FROM scratch
COPY sh /bin/sh
WORKDIR /work
RUN --mount=type=bind,from=builder,source=/artifact.txt,target=artifact.txt cat artifact.txt > /out.txt
"#,
        )
        .unwrap();

        let listener = UnixListener::bind(&socket).unwrap();
        let daemon = tokio::spawn(async move {
            let mut lease_count = 0usize;
            let mut exec_count = 0usize;
            let mut release_count = 0usize;
            let mut rootfs_hosts: HashMap<String, PathBuf> = HashMap::new();

            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let req: PoolRequest =
                    serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();

                match req {
                    PoolRequest::Lease(req) => {
                        lease_count += 1;
                        assert_eq!(req.volumes.len(), 1);
                        let (host, guest, mode) = parse_pool_volume_spec(&req.volumes[0]);
                        assert_eq!(guest, "/run/a3s/test-rootfs");
                        assert_eq!(mode, "rw");
                        let lease_id = format!("lease-{lease_count}");
                        rootfs_hosts.insert(lease_id.clone(), host);
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolLeaseResponse {
                                lease_id: Some(lease_id),
                                error: None,
                            })
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    PoolRequest::Exec(req) => {
                        exec_count += 1;
                        let rootfs = rootfs_hosts
                            .get(&req.lease_id)
                            .expect("lease records rootfs host");
                        match req.lease_id.as_str() {
                            "lease-1" => {
                                assert_eq!(
                                    req.cmd,
                                    vec![
                                        "/bin/sh".to_string(),
                                        "-c".to_string(),
                                        "printf built > /artifact.txt".to_string(),
                                    ]
                                );
                                std::fs::write(rootfs.join("artifact.txt"), "built").unwrap();
                            }
                            "lease-2" => {
                                assert_eq!(
                                    req.cmd,
                                    vec![
                                        "/bin/sh".to_string(),
                                        "-c".to_string(),
                                        "cd '/work' && cat artifact.txt > /out.txt".to_string(),
                                    ]
                                );
                                assert_eq!(
                                    std::fs::read_to_string(rootfs.join("work/artifact.txt"))
                                        .unwrap(),
                                    "built"
                                );
                                std::fs::write(rootfs.join("out.txt"), "built").unwrap();
                                std::fs::write(rootfs.join("work/artifact.txt"), "discarded")
                                    .unwrap();
                            }
                            other => panic!("unexpected lease id {other}"),
                        }

                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolRunResponse {
                                stdout: Vec::new(),
                                stderr: Vec::new(),
                                exit_code: 0,
                                error: None,
                            })
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    PoolRequest::Release(req) => {
                        release_count += 1;
                        assert!(matches!(req.lease_id.as_str(), "lease-1" | "lease-2"));
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolLeaseReleaseResponse { error: None }).unwrap(),
                        )
                        .await
                        .unwrap();
                        if release_count == 2 {
                            break;
                        }
                    }
                    other => panic!(
                        "unexpected pool request: {:?}",
                        std::mem::discriminant(&other)
                    ),
                }
            }

            assert_eq!(lease_count, 2);
            assert_eq!(exec_count, 2);
            assert_eq!(release_count, 2);
        });

        let store = Arc::new(ImageStore::new(&store_dir, 1024 * 1024 * 100).unwrap());
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            build(
                BuildConfig {
                    context_dir: context.clone(),
                    dockerfile_path: context.join("Dockerfile"),
                    tag: Some("run-pool-stage-bind:latest".to_string()),
                    build_args: HashMap::new(),
                    quiet: true,
                    platforms: vec![],
                    target: None,
                    no_cache: true,
                    metrics: None,
                    run_pool: Some(BuildRunPoolConfig {
                        socket: socket.to_string_lossy().to_string(),
                        image: Some("helper:latest".to_string()),
                        vcpus: 2,
                        memory_mb: 512,
                        guest_rootfs: "/run/a3s/test-rootfs".to_string(),
                        timeout_ns: 12_000_000_000,
                        run_cache_dir: run_cache_dir.clone(),
                    }),
                },
                store.clone(),
            ),
        )
        .await
        .expect("build should not hang")
        .unwrap();

        daemon.await.unwrap();
        assert_eq!(result.layer_count, 2);

        let stored = store.get("run-pool-stage-bind:latest").await.unwrap();
        let image = OciImage::from_path(&stored.path).unwrap();
        let files = image_layer_files(&image);
        assert!(files.iter().any(|path| path == "bin/sh"));
        assert!(files.iter().any(|path| path == "out.txt"));
        assert!(!files.iter().any(|path| path == "work/artifact.txt"));
    }

    #[cfg(all(feature = "pool", not(windows)))]
    #[tokio::test]
    async fn test_build_run_pool_bind_mount_from_external_image_is_not_committed() {
        use super::super::BuildRunPoolConfig;
        use crate::pool::client::{read_frame, write_frame};
        use crate::pool::{
            PoolLeaseReleaseResponse, PoolLeaseResponse, PoolRequest, PoolRunResponse,
        };
        use std::time::Duration;
        use tokio::net::UnixListener;

        let tmp = tempfile::TempDir::new().unwrap();
        let source_context = tmp.path().join("source-context");
        let target_context = tmp.path().join("target-context");
        let store_dir = tmp.path().join("images");
        let socket = tmp.path().join("pool.sock");
        let run_cache_dir = tmp.path().join("run-cache");
        std::fs::create_dir_all(&source_context).unwrap();
        std::fs::create_dir_all(&target_context).unwrap();
        std::fs::write(source_context.join("artifact.txt"), "from-external").unwrap();
        std::fs::write(
            source_context.join("Dockerfile"),
            r#"FROM scratch
COPY artifact.txt /artifact.txt
"#,
        )
        .unwrap();
        std::fs::write(target_context.join("sh"), "fake shell").unwrap();
        std::fs::write(
            target_context.join("Dockerfile"),
            r#"FROM scratch
COPY sh /bin/sh
WORKDIR /work
RUN --mount=type=bind,from=external-bind-source:latest,source=/artifact.txt,target=artifact.txt cat artifact.txt > /out.txt
"#,
        )
        .unwrap();

        let store = Arc::new(ImageStore::new(&store_dir, 1024 * 1024 * 100).unwrap());
        build(
            BuildConfig {
                context_dir: source_context.clone(),
                dockerfile_path: source_context.join("Dockerfile"),
                tag: Some("external-bind-source:latest".to_string()),
                build_args: HashMap::new(),
                quiet: true,
                platforms: vec![],
                target: None,
                no_cache: true,
                metrics: None,
                run_pool: None,
            },
            store.clone(),
        )
        .await
        .expect("source image should build into the local store");

        let listener = UnixListener::bind(&socket).unwrap();
        let daemon = tokio::spawn(async move {
            let mut rootfs_host: Option<PathBuf> = None;

            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let req: PoolRequest =
                    serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();

                match req {
                    PoolRequest::Lease(req) => {
                        assert_eq!(req.volumes.len(), 1);
                        let (host, guest, mode) = parse_pool_volume_spec(&req.volumes[0]);
                        assert_eq!(guest, "/run/a3s/test-rootfs");
                        assert_eq!(mode, "rw");
                        rootfs_host = Some(host);
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolLeaseResponse {
                                lease_id: Some("lease-external-bind".to_string()),
                                error: None,
                            })
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    PoolRequest::Exec(req) => {
                        assert_eq!(req.lease_id, "lease-external-bind");
                        assert_eq!(
                            req.cmd,
                            vec![
                                "/bin/sh".to_string(),
                                "-c".to_string(),
                                "cd '/work' && cat artifact.txt > /out.txt".to_string(),
                            ]
                        );
                        let rootfs = rootfs_host.as_ref().expect("lease records rootfs host");
                        assert_eq!(
                            std::fs::read_to_string(rootfs.join("work/artifact.txt")).unwrap(),
                            "from-external"
                        );
                        std::fs::write(rootfs.join("out.txt"), "from-external").unwrap();
                        std::fs::write(rootfs.join("work/artifact.txt"), "discarded").unwrap();
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolRunResponse {
                                stdout: Vec::new(),
                                stderr: Vec::new(),
                                exit_code: 0,
                                error: None,
                            })
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    PoolRequest::Release(req) => {
                        assert_eq!(req.lease_id, "lease-external-bind");
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolLeaseReleaseResponse { error: None }).unwrap(),
                        )
                        .await
                        .unwrap();
                        break;
                    }
                    other => panic!(
                        "unexpected pool request: {:?}",
                        std::mem::discriminant(&other)
                    ),
                }
            }
        });

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            build(
                BuildConfig {
                    context_dir: target_context.clone(),
                    dockerfile_path: target_context.join("Dockerfile"),
                    tag: Some("run-pool-external-bind:latest".to_string()),
                    build_args: HashMap::new(),
                    quiet: true,
                    platforms: vec![],
                    target: None,
                    no_cache: true,
                    metrics: None,
                    run_pool: Some(BuildRunPoolConfig {
                        socket: socket.to_string_lossy().to_string(),
                        image: Some("helper:latest".to_string()),
                        vcpus: 2,
                        memory_mb: 512,
                        guest_rootfs: "/run/a3s/test-rootfs".to_string(),
                        timeout_ns: 12_000_000_000,
                        run_cache_dir: run_cache_dir.clone(),
                    }),
                },
                store.clone(),
            ),
        )
        .await
        .expect("build should not hang")
        .unwrap();

        daemon.await.unwrap();
        assert_eq!(result.layer_count, 2);

        let stored = store.get("run-pool-external-bind:latest").await.unwrap();
        let image = OciImage::from_path(&stored.path).unwrap();
        let files = image_layer_files(&image);
        assert!(files.iter().any(|path| path == "bin/sh"));
        assert!(files.iter().any(|path| path == "out.txt"));
        assert!(!files.iter().any(|path| path == "work/artifact.txt"));
    }

    #[cfg(all(feature = "pool", not(windows)))]
    #[tokio::test]
    async fn test_build_run_pool_tmpfs_mount_is_not_committed() {
        use super::super::BuildRunPoolConfig;
        use crate::pool::client::{read_frame, write_frame};
        use crate::pool::{
            PoolLeaseReleaseResponse, PoolLeaseResponse, PoolRequest, PoolRunResponse,
        };
        use std::time::Duration;
        use tokio::net::UnixListener;

        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let store_dir = tmp.path().join("images");
        let socket = tmp.path().join("pool.sock");
        let run_cache_dir = tmp.path().join("run-cache");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("sh"), "fake shell").unwrap();
        std::fs::write(context.join("original.txt"), "from-rootfs\n").unwrap();
        std::fs::write(
            context.join("Dockerfile"),
            r#"FROM scratch
COPY sh /bin/sh
WORKDIR /work
COPY original.txt tmp/original.txt
RUN --mount=type=tmpfs,target=tmp printf ok > /out.txt
"#,
        )
        .unwrap();

        let listener = UnixListener::bind(&socket).unwrap();
        let daemon = tokio::spawn(async move {
            let mut rootfs_host: Option<PathBuf> = None;

            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let req: PoolRequest =
                    serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();

                match req {
                    PoolRequest::Lease(req) => {
                        let (host, guest, mode) = parse_pool_volume_spec(&req.volumes[0]);
                        assert_eq!(guest, "/run/a3s/test-rootfs");
                        assert_eq!(mode, "rw");
                        rootfs_host = Some(host);
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolLeaseResponse {
                                lease_id: Some("lease-tmpfs".to_string()),
                                error: None,
                            })
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    PoolRequest::Exec(req) => {
                        assert_eq!(req.lease_id, "lease-tmpfs");
                        assert_eq!(
                            req.cmd,
                            vec![
                                "/bin/sh".to_string(),
                                "-c".to_string(),
                                "cd '/work' && printf ok > /out.txt".to_string(),
                            ]
                        );
                        let rootfs = rootfs_host.as_ref().expect("lease records rootfs host");
                        assert!(rootfs.join("work/tmp").is_dir());
                        assert!(
                            !rootfs.join("work/tmp/original.txt").exists(),
                            "tmpfs mount should hide the original target during RUN"
                        );
                        std::fs::write(rootfs.join("out.txt"), "ok").unwrap();
                        std::fs::write(rootfs.join("work/tmp/transient.txt"), "discarded").unwrap();
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolRunResponse {
                                stdout: Vec::new(),
                                stderr: Vec::new(),
                                exit_code: 0,
                                error: None,
                            })
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    PoolRequest::Release(req) => {
                        assert_eq!(req.lease_id, "lease-tmpfs");
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolLeaseReleaseResponse { error: None }).unwrap(),
                        )
                        .await
                        .unwrap();
                        break;
                    }
                    other => panic!(
                        "unexpected pool request: {:?}",
                        std::mem::discriminant(&other)
                    ),
                }
            }
        });

        let store = Arc::new(ImageStore::new(&store_dir, 1024 * 1024 * 100).unwrap());
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            build(
                BuildConfig {
                    context_dir: context.clone(),
                    dockerfile_path: context.join("Dockerfile"),
                    tag: Some("run-pool-tmpfs:latest".to_string()),
                    build_args: HashMap::new(),
                    quiet: true,
                    platforms: vec![],
                    target: None,
                    no_cache: true,
                    metrics: None,
                    run_pool: Some(BuildRunPoolConfig {
                        socket: socket.to_string_lossy().to_string(),
                        image: Some("helper:latest".to_string()),
                        vcpus: 2,
                        memory_mb: 512,
                        guest_rootfs: "/run/a3s/test-rootfs".to_string(),
                        timeout_ns: 12_000_000_000,
                        run_cache_dir: run_cache_dir.clone(),
                    }),
                },
                store.clone(),
            ),
        )
        .await
        .expect("build should not hang")
        .unwrap();

        daemon.await.unwrap();
        assert_eq!(result.layer_count, 3);

        let stored = store.get("run-pool-tmpfs:latest").await.unwrap();
        let image = OciImage::from_path(&stored.path).unwrap();
        let files = image_layer_files(&image);
        assert!(files.iter().any(|path| path == "out.txt"));
        assert!(files.iter().any(|path| path == "work/tmp/original.txt"));
        assert!(!files.iter().any(|path| path == "work/tmp/transient.txt"));
    }

    #[cfg(all(feature = "pool", not(windows)))]
    #[tokio::test]
    async fn test_build_run_pool_releases_stage_lease_after_run_failure() {
        use super::super::BuildRunPoolConfig;
        use crate::pool::client::{read_frame, write_frame};
        use crate::pool::{
            PoolLeaseReleaseResponse, PoolLeaseResponse, PoolRequest, PoolRunResponse,
        };
        use std::time::Duration;
        use tokio::net::UnixListener;

        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let store_dir = tmp.path().join("images");
        let socket = tmp.path().join("pool.sock");
        let run_cache_dir = tmp.path().join("run-cache");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("sh"), "fake shell").unwrap();
        std::fs::write(context.join("cache-marker"), "original\n").unwrap();
        std::fs::write(
            context.join("Dockerfile"),
            r#"FROM scratch
COPY sh /bin/sh
COPY cache-marker /root/.cache/original.txt
RUN --mount=type=cache,id=failed,target=/root/.cache echo before-failure > /root/.cache/failed.txt && false
"#,
        )
        .unwrap();

        let listener = UnixListener::bind(&socket).unwrap();
        let daemon = tokio::spawn(async move {
            let mut lease_count = 0usize;
            let mut exec_count = 0usize;
            let mut release_count = 0usize;
            let mut rootfs_host: Option<PathBuf> = None;

            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let req: PoolRequest =
                    serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();

                match req {
                    PoolRequest::Lease(req) => {
                        lease_count += 1;
                        assert_eq!(req.volumes.len(), 1);
                        let (host, guest, mode) = parse_pool_volume_spec(&req.volumes[0]);
                        assert_eq!(guest, "/run/a3s/test-rootfs");
                        assert_eq!(mode, "rw");
                        rootfs_host = Some(host);
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolLeaseResponse {
                                lease_id: Some("lease-fail".to_string()),
                                error: None,
                            })
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    PoolRequest::Exec(req) => {
                        exec_count += 1;
                        assert_eq!(req.lease_id, "lease-fail");
                        let rootfs = rootfs_host.as_ref().expect("lease records rootfs host");
                        std::fs::write(rootfs.join("root/.cache/failed.txt"), "failed\n").unwrap();
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolRunResponse {
                                stdout: b"before-failure\n".to_vec(),
                                stderr: b"boom\n".to_vec(),
                                exit_code: 42,
                                error: None,
                            })
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    PoolRequest::Release(req) => {
                        release_count += 1;
                        assert_eq!(req.lease_id, "lease-fail");
                        let response =
                            serde_json::to_vec(&PoolLeaseReleaseResponse { error: None }).unwrap();
                        let _ = write_frame(&mut stream, &response).await;
                        break;
                    }
                    other => panic!(
                        "unexpected pool request: {:?}",
                        std::mem::discriminant(&other)
                    ),
                }
            }

            assert_eq!(lease_count, 1);
            assert_eq!(exec_count, 1);
            assert_eq!(release_count, 1);
        });

        let store = Arc::new(ImageStore::new(&store_dir, 1024 * 1024 * 100).unwrap());
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            build(
                BuildConfig {
                    context_dir: context.clone(),
                    dockerfile_path: context.join("Dockerfile"),
                    tag: Some("run-pool-failure:latest".to_string()),
                    build_args: HashMap::new(),
                    quiet: true,
                    platforms: vec![],
                    target: None,
                    no_cache: true,
                    metrics: None,
                    run_pool: Some(BuildRunPoolConfig {
                        socket: socket.to_string_lossy().to_string(),
                        image: Some("helper:latest".to_string()),
                        vcpus: 2,
                        memory_mb: 512,
                        guest_rootfs: "/run/a3s/test-rootfs".to_string(),
                        timeout_ns: 12_000_000_000,
                        run_cache_dir: run_cache_dir.clone(),
                    }),
                },
                store,
            ),
        )
        .await
        .expect("build should not hang")
        .expect_err("RUN failure should fail the build");

        assert!(error.to_string().contains("exit 42"));
        assert!(error.to_string().contains("boom"));
        daemon.await.unwrap();
        assert!(
            !tree_contains_file_named(&run_cache_dir, "failed.txt"),
            "failed RUN cache mount contents must not be persisted"
        );
    }

    #[cfg(all(feature = "pool", not(windows)))]
    #[tokio::test]
    async fn test_build_run_pool_cache_mount_is_not_committed_to_layer() {
        use super::super::BuildRunPoolConfig;
        use crate::pool::client::{read_frame, write_frame};
        use crate::pool::{
            PoolLeaseReleaseResponse, PoolLeaseResponse, PoolRequest, PoolRunResponse,
        };
        use std::time::Duration;
        use tokio::net::UnixListener;

        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let store_dir = tmp.path().join("images");
        let socket = tmp.path().join("pool.sock");
        let run_cache_dir = tmp.path().join("run-cache");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("sh"), "fake shell").unwrap();
        std::fs::write(context.join("cache-marker"), "original\n").unwrap();
        std::fs::write(
            context.join("Dockerfile"),
            r#"FROM scratch
COPY sh /bin/sh
COPY cache-marker /root/.cache/original.txt
RUN --mount=type=cache,id=warm,target=/root/.cache echo warm > /root/.cache/cache-only.txt
RUN --mount=type=cache,id=warm,target=/root/.cache cat /root/.cache/cache-only.txt > /result.txt
"#,
        )
        .unwrap();

        let listener = UnixListener::bind(&socket).unwrap();
        let daemon = tokio::spawn(async move {
            let mut rootfs_host: Option<PathBuf> = None;
            let mut exec_count = 0usize;

            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let req: PoolRequest =
                    serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();

                match req {
                    PoolRequest::Lease(req) => {
                        let (host, guest, mode) = parse_pool_volume_spec(&req.volumes[0]);
                        assert_eq!(guest, "/run/a3s/test-rootfs");
                        assert_eq!(mode, "rw");
                        rootfs_host = Some(host);
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolLeaseResponse {
                                lease_id: Some("lease-cache".to_string()),
                                error: None,
                            })
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    PoolRequest::Exec(req) => {
                        exec_count += 1;
                        assert_eq!(req.lease_id, "lease-cache");
                        assert_eq!(req.rootfs.as_deref(), Some("/run/a3s/test-rootfs"));
                        let rootfs = rootfs_host.as_ref().expect("lease records rootfs host");

                        assert!(
                            !rootfs.join("root/.cache/original.txt").exists(),
                            "cache mount should hide original target contents during RUN"
                        );
                        match exec_count {
                            1 => {
                                std::fs::write(rootfs.join("root/.cache/cache-only.txt"), "warm\n")
                                    .unwrap();
                            }
                            2 => {
                                let cached = std::fs::read_to_string(
                                    rootfs.join("root/.cache/cache-only.txt"),
                                )
                                .expect("second RUN sees persistent cache mount content");
                                std::fs::write(rootfs.join("result.txt"), cached).unwrap();
                            }
                            other => panic!("unexpected exec request {other}"),
                        }

                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolRunResponse {
                                stdout: Vec::new(),
                                stderr: Vec::new(),
                                exit_code: 0,
                                error: None,
                            })
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    PoolRequest::Release(req) => {
                        assert_eq!(req.lease_id, "lease-cache");
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolLeaseReleaseResponse { error: None }).unwrap(),
                        )
                        .await
                        .unwrap();
                        assert_eq!(exec_count, 2);
                        break;
                    }
                    other => panic!(
                        "unexpected pool request: {:?}",
                        std::mem::discriminant(&other)
                    ),
                }
            }
        });

        let store = Arc::new(ImageStore::new(&store_dir, 1024 * 1024 * 100).unwrap());
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            build(
                BuildConfig {
                    context_dir: context.clone(),
                    dockerfile_path: context.join("Dockerfile"),
                    tag: Some("run-pool-cache:latest".to_string()),
                    build_args: HashMap::new(),
                    quiet: true,
                    platforms: vec![],
                    target: None,
                    no_cache: true,
                    metrics: None,
                    run_pool: Some(BuildRunPoolConfig {
                        socket: socket.to_string_lossy().to_string(),
                        image: Some("helper:latest".to_string()),
                        vcpus: 2,
                        memory_mb: 512,
                        guest_rootfs: "/run/a3s/test-rootfs".to_string(),
                        timeout_ns: 60_000_000_000,
                        run_cache_dir,
                    }),
                },
                store.clone(),
            ),
        )
        .await
        .expect("build should not hang")
        .unwrap();

        daemon.await.unwrap();

        let stored = store.get("run-pool-cache:latest").await.unwrap();
        let image = OciImage::from_path(&stored.path).unwrap();
        let files = image_layer_files(&image);
        assert_eq!(result.layer_count, 3);
        assert!(files.iter().any(|path| path == "root/.cache/original.txt"));
        assert!(files.iter().any(|path| path == "result.txt"));
        assert!(!files
            .iter()
            .any(|path| path == "root/.cache/cache-only.txt"));
    }

    #[cfg(all(feature = "pool", not(windows)))]
    #[tokio::test]
    async fn test_build_run_pool_cache_mount_from_stage_seeds_cache() {
        use super::super::BuildRunPoolConfig;
        use crate::pool::client::{read_frame, write_frame};
        use crate::pool::{
            PoolLeaseReleaseResponse, PoolLeaseResponse, PoolRequest, PoolRunResponse,
        };
        use std::time::Duration;
        use tokio::net::UnixListener;

        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let store_dir = tmp.path().join("images");
        let socket = tmp.path().join("pool.sock");
        let run_cache_dir = tmp.path().join("run-cache");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("sh"), "fake shell").unwrap();
        std::fs::write(context.join("seed.txt"), "seeded\n").unwrap();
        std::fs::write(
            context.join("Dockerfile"),
            r#"FROM scratch AS builder
COPY seed.txt /seed-cache/seed.txt

FROM scratch
COPY sh /bin/sh
RUN --mount=type=cache,id=seeded,sharing=locked,from=builder,source=/seed-cache,target=/root/.cache cat /root/.cache/seed.txt > /out.txt
"#,
        )
        .unwrap();

        let listener = UnixListener::bind(&socket).unwrap();
        let daemon = tokio::spawn(async move {
            let mut rootfs_host: Option<PathBuf> = None;

            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let req: PoolRequest =
                    serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();

                match req {
                    PoolRequest::Lease(req) => {
                        assert_eq!(req.volumes.len(), 1);
                        let (host, guest, mode) = parse_pool_volume_spec(&req.volumes[0]);
                        assert_eq!(guest, "/run/a3s/test-rootfs");
                        assert_eq!(mode, "rw");
                        rootfs_host = Some(host);
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolLeaseResponse {
                                lease_id: Some("lease-cache-seed".to_string()),
                                error: None,
                            })
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    PoolRequest::Exec(req) => {
                        assert_eq!(req.lease_id, "lease-cache-seed");
                        assert_eq!(
                            req.cmd,
                            vec![
                                "/bin/sh".to_string(),
                                "-c".to_string(),
                                "cat /root/.cache/seed.txt > /out.txt".to_string(),
                            ]
                        );
                        let rootfs = rootfs_host.as_ref().expect("lease records rootfs host");
                        assert_eq!(
                            std::fs::read_to_string(rootfs.join("root/.cache/seed.txt")).unwrap(),
                            "seeded\n"
                        );
                        std::fs::write(rootfs.join("out.txt"), "seeded\n").unwrap();
                        std::fs::write(rootfs.join("root/.cache/generated.txt"), "persisted")
                            .unwrap();
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolRunResponse {
                                stdout: Vec::new(),
                                stderr: Vec::new(),
                                exit_code: 0,
                                error: None,
                            })
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    PoolRequest::Release(req) => {
                        assert_eq!(req.lease_id, "lease-cache-seed");
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&PoolLeaseReleaseResponse { error: None }).unwrap(),
                        )
                        .await
                        .unwrap();
                        break;
                    }
                    other => panic!(
                        "unexpected pool request: {:?}",
                        std::mem::discriminant(&other)
                    ),
                }
            }
        });

        let store = Arc::new(ImageStore::new(&store_dir, 1024 * 1024 * 100).unwrap());
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            build(
                BuildConfig {
                    context_dir: context.clone(),
                    dockerfile_path: context.join("Dockerfile"),
                    tag: Some("run-pool-cache-seed:latest".to_string()),
                    build_args: HashMap::new(),
                    quiet: true,
                    platforms: vec![],
                    target: None,
                    no_cache: true,
                    metrics: None,
                    run_pool: Some(BuildRunPoolConfig {
                        socket: socket.to_string_lossy().to_string(),
                        image: Some("helper:latest".to_string()),
                        vcpus: 2,
                        memory_mb: 512,
                        guest_rootfs: "/run/a3s/test-rootfs".to_string(),
                        timeout_ns: 60_000_000_000,
                        run_cache_dir: run_cache_dir.clone(),
                    }),
                },
                store.clone(),
            ),
        )
        .await
        .expect("build should not hang")
        .unwrap();

        daemon.await.unwrap();
        assert_eq!(result.layer_count, 2);

        let stored = store.get("run-pool-cache-seed:latest").await.unwrap();
        let image = OciImage::from_path(&stored.path).unwrap();
        let files = image_layer_files(&image);
        assert!(files.iter().any(|path| path == "bin/sh"));
        assert!(files.iter().any(|path| path == "out.txt"));
        assert!(!files.iter().any(|path| path == "root/.cache/seed.txt"));
        assert!(!files.iter().any(|path| path == "root/.cache/generated.txt"));
        assert!(tree_contains_file_named(&run_cache_dir, "seed.txt"));
        assert!(tree_contains_file_named(&run_cache_dir, "generated.txt"));
    }

    /// Regression: a multi-stage `COPY --from=<stage> /abs/path` must resolve
    /// the absolute source inside the source stage's rootfs. Previously
    /// `context_dir.join("/abs")` discarded the base (Path::join semantics) and
    /// looked at the host root, so multi-stage copies failed with "source not
    /// found".
    /// `--target <stage>` builds only up to the named stage and emits that
    /// stage's image (not the final stage), and never runs later stages.
    #[tokio::test]
    async fn test_build_target_stage() {
        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let store_dir = tmp.path().join("images");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("a.txt"), "a").unwrap();
        std::fs::write(context.join("b.txt"), "b").unwrap();
        std::fs::write(
            context.join("Dockerfile"),
            "FROM scratch AS builder\nCOPY a.txt /a.txt\nCMD [\"builder\"]\n\nFROM scratch\nCOPY b.txt /b.txt\nCMD [\"final\"]\n",
        )
        .unwrap();

        let store = Arc::new(ImageStore::new(&store_dir, 1024 * 1024 * 100).unwrap());
        let result = build(
            BuildConfig {
                context_dir: context.clone(),
                dockerfile_path: context.join("Dockerfile"),
                tag: Some("targeted:latest".to_string()),
                build_args: HashMap::new(),
                quiet: true,
                platforms: vec![],
                target: Some("builder".to_string()),
                no_cache: false,
                metrics: None,
                run_pool: None,
            },
            store.clone(),
        )
        .await
        .unwrap();

        // The output image is the `builder` stage: CMD ["builder"], and its
        // single layer contains a.txt (NOT b.txt from the final stage).
        let stored = store.get("targeted:latest").await.unwrap();
        let image = OciImage::from_path(&stored.path).unwrap();
        assert_eq!(image.config().cmd, Some(vec!["builder".to_string()]));
        assert_eq!(result.layer_count, 1);

        // An unknown --target is a clear error.
        let err = build(
            BuildConfig {
                context_dir: context.clone(),
                dockerfile_path: context.join("Dockerfile"),
                tag: Some("x:latest".to_string()),
                build_args: HashMap::new(),
                quiet: true,
                platforms: vec![],
                target: Some("nope".to_string()),
                no_cache: false,
                metrics: None,
                run_pool: None,
            },
            store.clone(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("target build stage 'nope' not found"));
    }

    /// `.dockerignore` must keep ignored context paths (secrets, `.git`,
    /// `node_modules`) out of `COPY .`, with `!` negation re-including.
    #[tokio::test]
    async fn test_build_honors_dockerignore() {
        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let store_dir = tmp.path().join("images");
        std::fs::create_dir_all(context.join(".git")).unwrap();
        std::fs::create_dir_all(context.join("logs")).unwrap();
        std::fs::write(context.join(".env"), "SECRET").unwrap();
        std::fs::write(context.join(".git/config"), "g").unwrap();
        std::fs::write(context.join("keep.txt"), "keep").unwrap();
        std::fs::write(context.join("logs/a.log"), "x").unwrap();
        std::fs::write(context.join("logs/important.log"), "y").unwrap();
        std::fs::write(
            context.join(".dockerignore"),
            ".git\n.env\n**/*.log\n!logs/important.log\n",
        )
        .unwrap();
        std::fs::write(context.join("Dockerfile"), "FROM scratch\nCOPY . /app\n").unwrap();

        let store = Arc::new(ImageStore::new(&store_dir, 1024 * 1024 * 100).unwrap());
        build(
            BuildConfig {
                context_dir: context.clone(),
                dockerfile_path: context.join("Dockerfile"),
                tag: Some("di:latest".to_string()),
                build_args: HashMap::new(),
                quiet: true,
                platforms: vec![],
                target: None,
                no_cache: false,
                metrics: None,
                run_pool: None,
            },
            store.clone(),
        )
        .await
        .unwrap();

        let stored = store.get("di:latest").await.unwrap();
        // Read the single layer and collect file paths.
        let image = OciImage::from_path(&stored.path).unwrap();
        let layer = &image.layer_paths()[0];
        let file = std::fs::File::open(layer).unwrap();
        let dec = flate2::read::GzDecoder::new(file);
        let mut ar = tar::Archive::new(dec);
        let names: Vec<String> = ar
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.header().entry_type().is_file())
            .map(|e| e.path().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(names.iter().any(|n| n == "app/keep.txt"));
        assert!(names.iter().any(|n| n == "app/logs/important.log")); // !negation
        assert!(
            !names.iter().any(|n| n.contains(".env")),
            "secret leaked: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains(".git")),
            ".git leaked: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "app/logs/a.log"),
            "*.log leaked: {names:?}"
        );
    }

    #[tokio::test]
    async fn test_build_multistage_copy_from_absolute_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let store_dir = tmp.path().join("images");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("run.sh"), "built-artifact").unwrap();
        std::fs::write(
            context.join("Dockerfile"),
            r#"FROM scratch AS builder
COPY run.sh /run.sh

FROM scratch
COPY --from=builder /run.sh /work/run.sh
CMD ["/work/run.sh"]
"#,
        )
        .unwrap();

        let store = Arc::new(ImageStore::new(&store_dir, 1024 * 1024 * 100).unwrap());
        let result = build(
            BuildConfig {
                context_dir: context.clone(),
                dockerfile_path: context.join("Dockerfile"),
                tag: Some("multistage:latest".to_string()),
                build_args: HashMap::new(),
                quiet: true,
                platforms: vec![],
                target: None,
                no_cache: false,
                metrics: None,
                run_pool: None,
            },
            store.clone(),
        )
        .await
        .expect("multi-stage COPY --from with an absolute source must build");

        // Only the final stage's single layer is in the output image.
        assert_eq!(result.layer_count, 1);
        let stored = store.get("multistage:latest").await.unwrap();
        let image = OciImage::from_path(&stored.path).unwrap();
        assert_eq!(image.config().cmd, Some(vec!["/work/run.sh".to_string()]));
    }

    #[tokio::test]
    async fn test_add_url_invalid_host_returns_error() {
        // Verify that ADD <url> with an unreachable host returns a BuildError,
        // not a silent skip. Uses a guaranteed-invalid host.
        use super::super::handlers::handle_add;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let layers = tmp.path().join("layers");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&layers).unwrap();

        let result = tokio::task::spawn_blocking(move || {
            handle_add(
                &["http://this-host-does-not-exist.invalid/file.txt".to_string()],
                "/tmp/file.txt",
                None,
                tmp.path(),
                &rootfs,
                &layers,
                "/",
                0,
                None,
            )
        })
        .await
        .unwrap();

        assert!(result.is_err(), "Expected error for unreachable URL");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("ADD URL download failed"),
            "Expected ADD URL error, got: {msg}"
        );
    }
}
