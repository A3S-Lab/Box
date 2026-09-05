# Compose Normalization

Box exposes Compose interpretation as a pure component in
`a3s_box_core::compose`. It accepts an in-memory ACL or YAML source plus an
explicit environment map and returns a typed `NormalizedComposeConfig`.

```rust
use std::collections::HashMap;

use a3s_box_core::compose::{
    normalize_compose, ComposeSourceFormat,
};

let source = r#"
service "api" {
  image = env("API_IMAGE")
  ports = ["8080:80/tcp"]
}
"#;
let environment = HashMap::from([(
    "API_IMAGE".to_string(),
    "ghcr.io/a3s/api:latest".to_string(),
)]);

let project = normalize_compose(
    source,
    ComposeSourceFormat::Acl,
    &environment,
)?;
assert_eq!(project.service_order()?, ["api"]);
# Ok::<(), Box<dyn std::error::Error>>(())
```

The normalizer does not read files, inspect process environment, create Box
resources, or mutate lifecycle state. Callers load source files and `.env`
content before invoking it. Relative `env_file` content remains a Runtime
translation concern because it depends on the caller-provided project base
directory.

`secret_environment` is deliberately different from interpolation. It maps a
guest variable to a caller process-environment variable name, and the
normalizer retains only that name:

```acl
service "migrator" {
  image = "ghcr.io/example/migrator:v1"
  secret_environment = {
    DATABASE_URL = "A3S_CLOUD_POSTGRES_MIGRATION_URL"
  }
}
```

Neither `${...}` nor `env(...)` is evaluated inside this map. Literal
`environment`/`env_file` values cannot collide with a Secret target. At
execution time the CLI reads each source only from its current process
environment, passes zeroizing material to Box's existing transient Secret
owner, and persists only read-only mount paths plus the non-sensitive guest
binding manifest. Linux execution requires the pre-mounted private
`<A3S_HOME>/runtime-secrets` tmpfs; other platforms fail before mutation.

## Deterministic Output

The normalized model has one representation for ACL and YAML alternatives:

- semantic maps use `BTreeMap`;
- the informational YAML `version` field is discarded;
- dependency and network map traversal is sorted;
- environment and label maps have stable key order;
- transient Secret references have stable key order but never contain values;
- TCP port suffixes normalize to the Runtime form;
- default volume and network drivers become explicit;
- network aliases are validated, sorted, and deduplicated.

`NormalizedComposeConfig::to_canonical_json` emits stable pretty JSON with a
final newline. ACL and YAML golden fixtures in
`src/core/tests/fixtures/compose/` must continue to produce identical bytes.

## Structured Diagnostics

Box uses a closed Compose schema. Unsupported YAML fields are collected instead
of being ignored, and ACL schema failures use the same diagnostic type.

```json
{
  "code": "compose.unsupported_field",
  "path": "/services/api/build",
  "message": "unsupported Compose field \"build\""
}
```

Every diagnostic has a stable code, a JSON Pointer-style path, and a message.
Parser diagnostics also include one-based line and column values when
available. Unsupported drivers and dependency conditions use
`compose.unsupported_value`; malformed recognized values use
`compose.invalid_value`.

## Runtime Boundary

`a3s_box_runtime::ComposeRuntimePlan` translates normalized configuration into
Box Runtime inputs. It contains no running-unit registry, persisted Box
lifecycle state, or Cloud desired state. The CLI owns local convergence and
cleanup; Cloud can reuse normalized interpretation without importing those
orchestration internals. Compose input is never Cloud's persisted desired-state
model.

For local CLI convergence, relative bind sources such as
`./config/app.conf:/etc/app.conf:ro` are resolved against the Compose file's
directory, including when `-f` points outside the current working directory.
The effective host paths participate in the service configuration digest and
are persisted for later `start` and `restart`. Named volumes retain their
VolumeStore identities; absolute and Windows drive/UNC bind paths are not
rebased.

Secret files use the same tmpfs validation, atomic materialization, guest
manifest, and cleanup lifecycle as the A3S Runtime provider. Compose adds only
reference projection and local convergence ownership; it does not introduce a
second Secret store or transport.
