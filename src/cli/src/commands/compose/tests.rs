use super::*;

#[test]
fn service_config_hash_is_stable_across_environment_order() {
    let service = ServiceConfig::default();
    let first = a3s_box_core::BoxConfig {
        image: "example:latest".to_string(),
        extra_env: vec![
            ("B".to_string(), "2".to_string()),
            ("A".to_string(), "1".to_string()),
        ],
        ..Default::default()
    };
    let mut second = first.clone();
    second.extra_env.reverse();

    assert_eq!(
        service_config_hash(&service, &first).unwrap(),
        service_config_hash(&service, &second).unwrap()
    );
}

#[test]
fn service_config_hash_tracks_runtime_isolation() {
    let service = ServiceConfig::default();
    let microvm = a3s_box_core::BoxConfig {
        image: "example:latest".to_string(),
        ..Default::default()
    };
    let mut sandbox = microvm.clone();
    sandbox.isolation = a3s_box_core::ExecutionIsolation::Sandbox;

    assert_ne!(
        service_config_hash(&service, &microvm).unwrap(),
        service_config_hash(&service, &sandbox).unwrap()
    );
}

#[test]
fn test_load_compose_file_not_found() {
    let result = load_compose_file(Some(std::path::Path::new("/nonexistent/compose.yaml")));
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not found"));
}

#[test]
fn test_load_compose_file_interpolates_dotenv_and_shell_before_validation() {
    let directory = tempfile::TempDir::new().unwrap();
    let compose_path = directory.path().join("compose.yaml");
    std::fs::write(
        &compose_path,
        r#"
services:
  redis:
    image: redis:7-alpine
    ports:
      - "${REDIS_PORT:-6379}:6379"
    environment:
      DASH_EMPTY: ${EMPTY-default}
      COLON_DASH_EMPTY: ${EMPTY:-default}
      PLUS_EMPTY: ${EMPTY+replacement}
      COLON_PLUS_EMPTY: ${EMPTY:+replacement}
      DOTENV_ONLY: ${DOTENV_ONLY}
      SHELL_WINS: ${SHELL_WINS}
"#,
    )
    .unwrap();
    std::fs::write(
        directory.path().join(".env"),
        "REDIS_PORT=6379\nEMPTY=\nDOTENV_ONLY=dotenv\nSHELL_WINS=dotenv\n",
    )
    .unwrap();
    let shell = HashMap::from([
        ("REDIS_PORT".to_string(), "16379".to_string()),
        ("SHELL_WINS".to_string(), "shell".to_string()),
    ]);

    let (_, config) = load_compose_file_with_environment(Some(&compose_path), shell).unwrap();
    let service = &config.services["redis"];
    assert_eq!(service.ports, vec!["16379:6379"]);
    let environment: HashMap<_, _> = service.environment.to_pairs().into_iter().collect();
    assert_eq!(environment["DASH_EMPTY"], "");
    assert_eq!(environment["COLON_DASH_EMPTY"], "default");
    assert_eq!(environment["PLUS_EMPTY"], "replacement");
    assert_eq!(environment["COLON_PLUS_EMPTY"], "");
    assert_eq!(environment["DOTENV_ONLY"], "dotenv");
    assert_eq!(environment["SHELL_WINS"], "shell");

    a3s_box_runtime::ComposeRuntimePlan::with_base_dir("test", config, directory.path())
        .expect("interpolation must run before port validation");
}

#[test]
fn test_load_compose_file_reports_unreadable_dotenv() {
    let directory = tempfile::TempDir::new().unwrap();
    let compose_path = directory.path().join("compose.yaml");
    std::fs::write(&compose_path, "services: {}\n").unwrap();
    std::fs::create_dir(directory.path().join(".env")).unwrap();

    let error =
        load_compose_file_with_environment(Some(&compose_path), HashMap::<String, String>::new())
            .unwrap_err();

    assert!(error
        .to_string()
        .contains("Failed to read Compose environment file"));
    assert!(error.to_string().contains(".env"));
}

#[test]
fn test_load_compose_file_rejects_unknown_yaml_fields_with_structured_path() {
    let directory = tempfile::TempDir::new().unwrap();
    let compose_path = directory.path().join("compose.yaml");
    std::fs::write(
        &compose_path,
        "services:\n  api:\n    image: api:latest\n    build: .\n",
    )
    .unwrap();

    let error =
        load_compose_file_with_environment(Some(&compose_path), HashMap::<String, String>::new())
            .unwrap_err();
    let message = error.to_string();

    assert!(message.contains("compose.unsupported_field"));
    assert!(message.contains("/services/api/build"));
}

#[test]
fn test_compose_files_constant() {
    assert_eq!(COMPOSE_FILES.len(), 5);
    assert_eq!(COMPOSE_FILES[0], "compose.acl");
    assert!(COMPOSE_FILES.contains(&"compose.yaml"));
    assert!(COMPOSE_FILES.contains(&"docker-compose.yml"));
}

#[test]
fn test_default_discovery_prefers_compose_acl() {
    let directory = tempfile::TempDir::new().unwrap();
    let acl_path = directory.path().join("compose.acl");
    let yaml_path = directory.path().join("compose.yaml");
    std::fs::write(&acl_path, "service \"api\" { image = \"api:latest\" }").unwrap();
    std::fs::write(&yaml_path, "services: {}\n").unwrap();

    let selected = resolve_compose_path(None, directory.path()).unwrap();

    assert_eq!(selected, acl_path);
}

#[test]
fn test_load_compose_acl_uses_dotenv_and_shell_environment() {
    let directory = tempfile::TempDir::new().unwrap();
    let compose_path = directory.path().join("compose.acl");
    std::fs::write(
        &compose_path,
        r#"service "api" {
  image = "api:${IMAGE_TAG}"
  environment = {
    FROM_DOTENV = env("FROM_DOTENV")
    SHELL_WINS = env("SHELL_WINS")
  }
}
"#,
    )
    .unwrap();
    std::fs::write(
        directory.path().join(".env"),
        "IMAGE_TAG=dotenv\nFROM_DOTENV=present\nSHELL_WINS=dotenv\n",
    )
    .unwrap();
    let shell = HashMap::from([
        ("IMAGE_TAG".to_string(), "shell".to_string()),
        ("SHELL_WINS".to_string(), "shell".to_string()),
    ]);

    let (_, config) = load_compose_file_with_environment(Some(&compose_path), shell).unwrap();
    let service = &config.services["api"];
    assert_eq!(service.image.as_deref(), Some("api:shell"));
    let environment: HashMap<_, _> = service.environment.to_pairs().into_iter().collect();
    assert_eq!(environment["FROM_DOTENV"], "present");
    assert_eq!(environment["SHELL_WINS"], "shell");
}

#[test]
fn test_label_constants() {
    assert_eq!(LABEL_PROJECT, "com.a3s.compose.project");
    assert_eq!(LABEL_SERVICE, "com.a3s.compose.service");
}

#[test]
fn test_service_restart_policy_normalizes_on_failure_limit() {
    let service = ServiceConfig {
        restart: Some("on-failure:3".to_string()),
        ..Default::default()
    };

    let (policy, max_count) = service_restart_policy("web", Some(&service)).unwrap();

    assert_eq!(policy, "on-failure");
    assert_eq!(max_count, 3);
}

#[test]
fn test_validate_compose_restart_policies_rejects_invalid_service_policy() {
    let mut services = HashMap::new();
    services.insert(
        "web".to_string(),
        ServiceConfig {
            image: Some("docker.io/library/alpine:latest".to_string()),
            restart: Some("never".to_string()),
            ..Default::default()
        },
    );
    let config = ComposeConfig {
        version: None,
        services,
        volumes: HashMap::new(),
        networks: HashMap::new(),
    };

    let error = validate_compose_restart_policies(&config).unwrap_err();

    assert!(error.contains("Service 'web' has invalid restart policy"));
    assert!(error.contains("Invalid restart policy"));
}

#[test]
fn test_service_box_from_record_captures_cleanup_fields() {
    let mut record = crate::test_helpers::fixtures::make_record(
        "compose-id",
        "project-web",
        "running",
        Some(123),
    );
    record
        .labels
        .insert(LABEL_SERVICE.to_string(), "web".to_string());
    record.network_name = Some("project_default".to_string());
    record.volume_names = vec!["data".to_string()];
    record.anonymous_volumes = vec!["anon".to_string()];
    record.stop_signal = Some("SIGINT".to_string());
    record.stop_timeout = Some(3);

    let service = ServiceBox::from_record(&record);

    assert_eq!(service.box_id, "compose-id");
    assert_eq!(service.svc_name, "web");
    assert_eq!(service.pid, Some(123));
    assert_eq!(service.network_name.as_deref(), Some("project_default"));
    assert_eq!(service.volume_names, vec!["data".to_string()]);
    assert_eq!(service.anonymous_volumes, vec!["anon".to_string()]);
    assert_eq!(service.stop_signal.as_deref(), Some("SIGINT"));
    assert_eq!(service.stop_timeout, Some(3));
    assert!(service.is_active());
}

#[test]
fn test_service_box_from_record_uses_network_mode_fallback() {
    let mut record =
        crate::test_helpers::fixtures::make_record("compose-id", "project-web", "running", None);
    record.network_name = None;
    record.network_mode = a3s_box_core::NetworkMode::Bridge {
        network: "legacy_default".to_string(),
    };

    let service = ServiceBox::from_record(&record);

    assert_eq!(service.network_name.as_deref(), Some("legacy_default"));
}

#[test]
fn test_rollback_with_current_appends_current_service() {
    let mut first_record =
        crate::test_helpers::fixtures::make_record("first-id", "project-db", "running", None);
    first_record
        .labels
        .insert(LABEL_SERVICE.to_string(), "db".to_string());
    let mut current_record =
        crate::test_helpers::fixtures::make_record("current-id", "project-web", "running", None);
    current_record
        .labels
        .insert(LABEL_SERVICE.to_string(), "web".to_string());

    let first = ServiceBox::from_record(&first_record);
    let current = ServiceBox::from_record(&current_record);
    let rollback_services = rollback_with_current(&[first], current);

    assert_eq!(rollback_services.len(), 2);
    assert_eq!(rollback_services[0].svc_name, "db");
    assert_eq!(rollback_services[1].svc_name, "web");
}

#[test]
fn test_service_box_paused_is_active() {
    let mut record =
        crate::test_helpers::fixtures::make_record("compose-id", "project-web", "paused", None);
    record
        .labels
        .insert(LABEL_SERVICE.to_string(), "web".to_string());

    let service = ServiceBox::from_record(&record);

    assert!(service.is_active());
}

#[test]
fn test_service_box_stopped_is_not_active() {
    let mut record =
        crate::test_helpers::fixtures::make_record("compose-id", "project-web", "stopped", None);
    record
        .labels
        .insert(LABEL_SERVICE.to_string(), "web".to_string());

    let service = ServiceBox::from_record(&record);

    assert!(!service.is_active());
}

#[test]
fn test_partial_service_cleanup_removes_box_directory() {
    let directory = tempfile::TempDir::new().unwrap();
    let box_dir = directory.path().join("box");
    let exec_socket = box_dir.join("sockets").join("exec.sock");
    std::fs::create_dir_all(box_dir.join("rootfs")).unwrap();
    std::fs::create_dir_all(exec_socket.parent().unwrap()).unwrap();
    std::fs::write(box_dir.join("rootfs").join("partial"), "data").unwrap();

    cleanup_partial_service_box("partial-id", &box_dir, &exec_socket, None, &[], &[]);

    assert!(!box_dir.exists());
}
