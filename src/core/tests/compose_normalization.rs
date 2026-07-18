use std::collections::HashMap;

use a3s_box_core::compose::{normalize_compose, ComposeDiagnosticCode, ComposeSourceFormat};

const YAML_FIXTURE: &str = include_str!("fixtures/compose/canonical.yaml");
const ACL_FIXTURE: &str = include_str!("fixtures/compose/canonical.acl");
const NORMALIZED_FIXTURE: &str = include_str!("fixtures/compose/canonical.normalized.json");

fn environment() -> HashMap<String, String> {
    HashMap::from([(
        "API_IMAGE".to_string(),
        "ghcr.io/a3s/api:latest".to_string(),
    )])
}

#[test]
fn yaml_and_acl_normalize_to_the_same_golden_document() {
    let environment = environment();
    let yaml = normalize_compose(YAML_FIXTURE, ComposeSourceFormat::Yaml, &environment)
        .expect("normalize YAML fixture");
    let acl = normalize_compose(ACL_FIXTURE, ComposeSourceFormat::Acl, &environment)
        .expect("normalize ACL fixture");

    assert_eq!(yaml, acl);
    assert_eq!(
        yaml.to_canonical_json().expect("serialize normalized YAML"),
        NORMALIZED_FIXTURE
    );
    assert_eq!(
        yaml.service_order().expect("deterministic service order"),
        ["db", "api"]
    );
}

#[test]
fn normalization_is_stable_across_repeated_runs() {
    let environment = environment();
    let expected = normalize_compose(YAML_FIXTURE, ComposeSourceFormat::Yaml, &environment)
        .expect("normalize fixture")
        .to_canonical_json()
        .expect("serialize fixture");

    for _ in 0..32 {
        let actual = normalize_compose(YAML_FIXTURE, ComposeSourceFormat::Yaml, &environment)
            .expect("normalize fixture")
            .to_canonical_json()
            .expect("serialize fixture");
        assert_eq!(actual, expected);
    }
}

#[test]
fn yaml_unknown_fields_return_sorted_structured_diagnostics() {
    let source = r#"
name: silently-ignored-before
services:
  db:
    image: postgres:17
  api:
    image: api:latest
    build: .
    deploy:
      replicas: 2
    depends_on:
      db:
        restart: true
    networks:
      backend:
        priority: 10
    healthcheck:
      test: ["CMD", "true"]
      grace_period: 5s
volumes:
  data:
    external: true
networks:
  backend:
    external: true
"#;

    let error = normalize_compose(source, ComposeSourceFormat::Yaml, &HashMap::new())
        .expect_err("unsupported fields must fail normalization");
    let diagnostics = error.diagnostics();

    assert!(diagnostics
        .iter()
        .all(|diagnostic| diagnostic.code == ComposeDiagnosticCode::UnsupportedField));
    assert_eq!(
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.path.as_str())
            .collect::<Vec<_>>(),
        [
            "/name",
            "/networks/backend/external",
            "/services/api/build",
            "/services/api/depends_on/db/restart",
            "/services/api/deploy",
            "/services/api/healthcheck/grace_period",
            "/services/api/networks/backend/priority",
            "/volumes/data/external",
        ]
    );
}

#[test]
fn acl_unknown_fields_return_a_structured_diagnostic() {
    let error = normalize_compose(
        r#"service "api" { image = "api:latest" build = "." }"#,
        ComposeSourceFormat::Acl,
        &HashMap::new(),
    )
    .expect_err("unsupported ACL field must fail normalization");

    assert_eq!(error.diagnostics().len(), 1);
    assert_eq!(
        error.diagnostics()[0].code,
        ComposeDiagnosticCode::UnsupportedField
    );
    assert_eq!(error.diagnostics()[0].path, "/services/api/build");
    assert_eq!(
        serde_json::to_value(&error.diagnostics()[0]).unwrap()["code"],
        "compose.unsupported_field"
    );
}

#[test]
fn unsupported_driver_values_return_structured_diagnostics() {
    let error = normalize_compose(
        r#"
services:
  api:
    image: api:latest
volumes:
  data:
    driver: nfs
networks:
  backend:
    driver: overlay
"#,
        ComposeSourceFormat::Yaml,
        &HashMap::new(),
    )
    .expect_err("unsupported drivers must fail normalization");

    assert_eq!(
        error
            .diagnostics()
            .iter()
            .map(|diagnostic| (&diagnostic.code, diagnostic.path.as_str()))
            .collect::<Vec<_>>(),
        [
            (
                &ComposeDiagnosticCode::UnsupportedValue,
                "/networks/backend/driver",
            ),
            (
                &ComposeDiagnosticCode::UnsupportedValue,
                "/volumes/data/driver",
            ),
        ]
    );
}

#[test]
fn unsupported_multi_network_runtime_shape_fails_before_lifecycle_work() {
    let error = normalize_compose(
        r#"
services:
  api:
    image: api:latest
    networks: [frontend, backend]
"#,
        ComposeSourceFormat::Yaml,
        &HashMap::new(),
    )
    .expect_err("a service cannot be partially attached to multiple networks");

    assert_eq!(error.diagnostics().len(), 1);
    assert_eq!(
        error.diagnostics()[0].code,
        ComposeDiagnosticCode::UnsupportedValue
    );
    assert_eq!(error.diagnostics()[0].path, "/services/api/networks");
}
