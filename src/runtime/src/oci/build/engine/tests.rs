//! Tests for the build engine.

#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use super::super::utils::*;
    use std::collections::HashMap;

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
    fn test_format_size() {
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1_500_000), "1.4 MB");
        assert_eq!(format_size(1_500_000_000), "1.4 GB");
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
