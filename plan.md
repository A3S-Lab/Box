# Plan: 9.3 Registry Push (P1)

## Context

`oci-distribution` 0.11 already supports `push()`, `push_blob()`, `push_manifest()`. The pull infrastructure (`RegistryPuller`, `RegistryAuth`, `ImageReference`, `ImageStore`) is mature. We need to add push, login, and logout.

## Scope

1. **Credential Store** — Persistent per-registry credentials at `~/.a3s/auth/credentials.json`
2. **RegistryPusher** — Push local OCI images to registries using `oci-distribution`
3. **CLI: `a3s-box push`** — Push a local image to a registry
4. **CLI: `a3s-box login`** — Store registry credentials
5. **CLI: `a3s-box logout`** — Remove stored credentials
6. **Wire RegistryAuth** — Load credentials from store (fallback to env vars)

Image signing (cosign/notation) is deferred — it's a separate concern.

---

## Feature 1: Credential Store

**File:** `runtime/src/oci/credentials.rs` (new)

```rust
pub struct CredentialStore { path: PathBuf }

impl CredentialStore {
    pub fn default_path() -> Result<Self>       // ~/.a3s/auth/credentials.json
    pub fn store(&self, registry: &str, username: &str, password: &str) -> Result<()>
    pub fn get(&self, registry: &str) -> Result<Option<(String, String)>>
    pub fn remove(&self, registry: &str) -> Result<bool>
    pub fn list_registries(&self) -> Result<Vec<String>>
}
```

- JSON format: `{ "registries": { "docker.io": { "username": "...", "password": "..." }, ... } }`
- Atomic writes (write tmp, rename)
- Creates parent directory on first use
- Tests: store/get/remove/list, overwrite existing, remove nonexistent

## Feature 2: RegistryPusher

**File:** `runtime/src/oci/registry.rs` (extend existing)

```rust
pub struct RegistryPusher { client: Client, auth: RegistryAuth }

impl RegistryPusher {
    pub fn new() -> Self
    pub fn with_auth(auth: RegistryAuth) -> Self
    pub async fn push(&self, reference: &ImageReference, image_dir: &Path) -> Result<PushResult>
}

pub struct PushResult { pub manifest_url: String, pub config_url: String }
```

Push workflow:
1. Read OCI layout from `image_dir` (index.json -> manifest -> config + layers)
2. Read each layer blob from `blobs/sha256/`
3. Read config blob
4. Call `client.push()` with `ImageLayer` vec, `Config`, and auth
5. Return URLs

Also extend `RegistryAuth`:
```rust
pub fn from_credential_store(registry: &str) -> Self
```

Tests: `to_oci_reference` for push, `from_credential_store` fallback chain

## Feature 3: CLI `push` command

**File:** `cli/src/commands/push.rs` (new)

```rust
pub struct PushArgs {
    pub image: String,
    #[arg(short, long)]
    pub quiet: bool,
}
```

Workflow:
1. Parse image reference
2. Look up image in local ImageStore
3. Load auth from CredentialStore (for target registry)
4. Create RegistryPusher with auth
5. Push image directory to registry
6. Print digest/URL

## Feature 4: CLI `login` command

**File:** `cli/src/commands/login.rs` (new)

```rust
pub struct LoginArgs {
    pub server: Option<String>,
    #[arg(short, long)]
    pub username: Option<String>,
    #[arg(short, long)]
    pub password: Option<String>,
    #[arg(long)]
    pub password_stdin: bool,
}
```

## Feature 5: CLI `logout` command

**File:** `cli/src/commands/logout.rs` (new)

```rust
pub struct LogoutArgs {
    pub server: Option<String>,
}
```

---

## Files Changed

| File | Change |
|------|--------|
| `runtime/src/oci/credentials.rs` | New — CredentialStore |
| `runtime/src/oci/registry.rs` | Add RegistryPusher, extend RegistryAuth |
| `runtime/src/oci/mod.rs` | Export new types |
| `runtime/src/lib.rs` | Re-export CredentialStore, RegistryPusher |
| `cli/src/commands/push.rs` | New — push command |
| `cli/src/commands/login.rs` | New — login command |
| `cli/src/commands/logout.rs` | New — logout command |
| `cli/src/commands/mod.rs` | Register push/login/logout commands |
| `cli/src/commands/pull.rs` | Use CredentialStore for auth |
| `crates/box/README.md` | Update roadmap 9.3, features |
| `README.md` | Update progress |

## Implementation Order

1. Credential Store (isolated, pure TDD)
2. Extend RegistryAuth with credential store loading
3. RegistryPusher (extends existing registry.rs)
4. CLI login/logout (simple, uses CredentialStore)
5. CLI push (uses RegistryPusher + ImageStore + CredentialStore)
6. Wire pull command to use CredentialStore
7. Documentation updates
