//! Persistent credential store for container registries.
//!
//! Stores per-registry credentials at `~/.a3s/auth/credentials.json`.
//! Uses atomic writes (write tmp, rename) for safety.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// Per-registry credential entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CredentialEntry {
    username: String,
    password: String,
}

/// Persistent credential file format.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CredentialFile {
    registries: HashMap<String, CredentialEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct DockerConfigFile {
    #[serde(default)]
    auths: HashMap<String, DockerAuthEntry>,
    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
    #[serde(default, rename = "credHelpers")]
    cred_helpers: HashMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct DockerAuthEntry {
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default, rename = "identitytoken")]
    identity_token: Option<String>,
}

/// Persistent credential store for container registries.
///
/// Stores credentials at `~/.a3s/auth/credentials.json`.
pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    /// Create a credential store at the default path (`~/.a3s/auth/credentials.json`).
    pub fn default_path() -> Result<Self> {
        Ok(Self {
            path: a3s_box_core::dirs_home()
                .join("auth")
                .join("credentials.json"),
        })
    }

    /// Create a credential store at a custom path.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Store credentials for a registry. Overwrites existing entry.
    pub fn store(&self, registry: &str, username: &str, password: &str) -> Result<()> {
        self.with_write_lock(|file| {
            file.registries.insert(
                normalize_registry(registry),
                CredentialEntry {
                    username: username.to_string(),
                    password: password.to_string(),
                },
            );
            Ok(())
        })
    }

    /// Get credentials for a registry. Returns `(username, password)`.
    pub fn get(&self, registry: &str) -> Result<Option<(String, String)>> {
        let file = self.load()?;
        Ok(file
            .registries
            .get(&normalize_registry(registry))
            .map(|e| (e.username.clone(), e.password.clone())))
    }

    /// Remove credentials for a registry. Returns true if entry existed.
    pub fn remove(&self, registry: &str) -> Result<bool> {
        self.with_write_lock(|file| {
            Ok(file
                .registries
                .remove(&normalize_registry(registry))
                .is_some())
        })
    }

    /// List all registries with stored credentials.
    pub fn list_registries(&self) -> Result<Vec<String>> {
        let file = self.load()?;
        let mut registries: Vec<String> = file.registries.keys().cloned().collect();
        registries.sort();
        Ok(registries)
    }

    /// Load the credential file from disk. Returns empty if not found.
    fn load(&self) -> Result<CredentialFile> {
        if !self.path.exists() {
            return Ok(CredentialFile::default());
        }
        let data = std::fs::read_to_string(&self.path).map_err(|e| {
            BoxError::ConfigError(format!(
                "Failed to read credential store {}: {}",
                self.path.display(),
                e
            ))
        })?;
        serde_json::from_str(&data).map_err(|e| {
            BoxError::ConfigError(format!(
                "Failed to parse credential store {}: {}",
                self.path.display(),
                e
            ))
        })
    }

    /// Save the credential file to disk atomically (write tmp, rename).
    fn save(&self, file: &CredentialFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxError::ConfigError(format!(
                    "Failed to create credential store directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        let tmp_path = self.path.with_extension("tmp");
        let data = serde_json::to_string_pretty(file)?;
        std::fs::write(&tmp_path, &data).map_err(|e| {
            BoxError::ConfigError(format!(
                "Failed to write credential store {}: {}",
                tmp_path.display(),
                e
            ))
        })?;
        std::fs::rename(&tmp_path, &self.path).map_err(|e| {
            BoxError::ConfigError(format!(
                "Failed to rename credential store {} -> {}: {}",
                tmp_path.display(),
                self.path.display(),
                e
            ))
        })?;
        Ok(())
    }

    /// Run `f` over the credential file under a cross-process advisory lock,
    /// re-loading fresh inside the lock and saving the result.
    ///
    /// `store`/`remove` funnel through here so two concurrent `a3s-box login`
    /// processes cannot lose each other's entry: the atomic tmp+rename in
    /// `save` only prevents a torn read, and `save` even uses a fixed `.tmp`
    /// name that concurrent writers would collide on. `save` stays lock-free —
    /// the guard is held here across the whole load → mutate → save and is
    /// non-reentrant.
    fn with_write_lock<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut CredentialFile) -> Result<R>,
    {
        let _lock = crate::file_lock::FileLock::acquire(&self.path).map_err(|e| {
            BoxError::ConfigError(format!(
                "Failed to lock credential store {}: {e}",
                self.path.display()
            ))
        })?;
        let mut file = self.load()?;
        let r = f(&mut file)?;
        self.save(&file)?;
        Ok(r)
    }
}

/// Get credentials from Docker's config (`~/.docker/config.json` or
/// `$DOCKER_CONFIG/config.json`), including credential helpers.
pub(crate) fn docker_credentials(registry: &str) -> Option<(String, String)> {
    let config_path = docker_config_path()?;
    let config = load_docker_config(&config_path).ok()?;
    let candidates = docker_registry_candidates(registry);

    if let Some((key, helper)) = matching_credential_helper(&config, &candidates) {
        if let Some(creds) =
            docker_credential_helper_get(helper, &helper_server_candidates(key, registry))
        {
            return Some(creds);
        }
    }

    if let Some(helper) = config.creds_store.as_deref() {
        if let Some(creds) = docker_credential_helper_get(helper, &candidates) {
            return Some(creds);
        }
    }

    matching_docker_auth(&config, &candidates).and_then(docker_auth_entry_credentials)
}

fn docker_config_path() -> Option<PathBuf> {
    if let Ok(config_dir) = std::env::var("DOCKER_CONFIG") {
        return Some(PathBuf::from(config_dir).join("config.json"));
    }
    dirs::home_dir().map(|home| home.join(".docker").join("config.json"))
}

fn load_docker_config(path: &Path) -> Result<DockerConfigFile> {
    let data = std::fs::read_to_string(path).map_err(|e| {
        BoxError::ConfigError(format!(
            "Failed to read Docker credential config {}: {}",
            path.display(),
            e
        ))
    })?;
    serde_json::from_str(&data).map_err(|e| {
        BoxError::ConfigError(format!(
            "Failed to parse Docker credential config {}: {}",
            path.display(),
            e
        ))
    })
}

fn docker_registry_candidates(registry: &str) -> Vec<String> {
    let normalized = normalize_registry(registry);
    let mut candidates = vec![
        registry.trim().to_string(),
        normalized.clone(),
        format!("https://{}", registry.trim()),
        format!("http://{}", registry.trim()),
        format!("https://{normalized}"),
        format!("http://{normalized}"),
    ];

    if normalized == "index.docker.io" {
        candidates.extend(
            [
                "docker.io",
                "registry-1.docker.io",
                "https://index.docker.io/v1/",
                "https://index.docker.io/v1",
                "index.docker.io/v1/",
                "index.docker.io/v1",
            ]
            .into_iter()
            .map(str::to_string),
        );
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

fn matching_credential_helper<'a>(
    config: &'a DockerConfigFile,
    candidates: &[String],
) -> Option<(&'a str, &'a str)> {
    config
        .cred_helpers
        .iter()
        .find(|(key, _)| registry_key_matches(key, candidates))
        .map(|(key, helper)| (key.as_str(), helper.as_str()))
}

fn matching_docker_auth<'a>(
    config: &'a DockerConfigFile,
    candidates: &[String],
) -> Option<&'a DockerAuthEntry> {
    config
        .auths
        .iter()
        .find(|(key, _)| registry_key_matches(key, candidates))
        .map(|(_, entry)| entry)
}

fn registry_key_matches(key: &str, candidates: &[String]) -> bool {
    let key_norm = normalize_docker_server_key(key);
    candidates
        .iter()
        .any(|candidate| key == candidate || key_norm == normalize_docker_server_key(candidate))
}

fn normalize_docker_server_key(value: &str) -> String {
    let trimmed = value.trim().trim_end_matches('/');
    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let without_v1 = without_scheme.strip_suffix("/v1").unwrap_or(without_scheme);
    normalize_registry(without_v1)
}

fn helper_server_candidates(matched_key: &str, registry: &str) -> Vec<String> {
    let mut candidates = vec![matched_key.to_string()];
    candidates.extend(docker_registry_candidates(registry));
    candidates.sort();
    candidates.dedup();
    candidates
}

fn docker_auth_entry_credentials(entry: &DockerAuthEntry) -> Option<(String, String)> {
    if let (Some(username), Some(password)) = (&entry.username, &entry.password) {
        if !username.is_empty() && !password.is_empty() {
            return Some((username.clone(), password.clone()));
        }
    }

    if let Some(auth) = entry.auth.as_deref() {
        if let Some(creds) = decode_docker_auth(auth) {
            return Some(creds);
        }
    }

    entry
        .identity_token
        .as_ref()
        .filter(|token| !token.is_empty())
        .map(|token| ("oauth2accesstoken".to_string(), token.clone()))
}

fn decode_docker_auth(auth: &str) -> Option<(String, String)> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(auth.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (username, password) = text.split_once(':')?;
    if username.is_empty() || password.is_empty() {
        return None;
    }
    Some((username.to_string(), password.to_string()))
}

fn docker_credential_helper_get(
    helper: &str,
    server_candidates: &[String],
) -> Option<(String, String)> {
    for server in server_candidates {
        if let Some(creds) = docker_credential_helper_get_one(helper, server) {
            return Some(creds);
        }
    }
    None
}

fn docker_credential_helper_get_one(helper: &str, server: &str) -> Option<(String, String)> {
    let program = format!("docker-credential-{helper}");
    let mut child = std::process::Command::new(program)
        .arg("get")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    {
        use std::io::Write as _;
        let stdin = child.stdin.as_mut()?;
        if stdin.write_all(server.as_bytes()).is_err() {
            let _ = child.kill();
            return None;
        }
    }

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }

    #[derive(Deserialize)]
    #[allow(non_snake_case)]
    struct HelperResponse {
        Username: String,
        Secret: String,
    }

    let response: HelperResponse = serde_json::from_slice(&output.stdout).ok()?;
    if response.Username.is_empty() || response.Secret.is_empty() {
        return None;
    }
    Some((response.Username, response.Secret))
}

/// Normalize registry names (e.g., "docker.io" and "index.docker.io" → "index.docker.io").
fn normalize_registry(registry: &str) -> String {
    let r = registry.trim().to_lowercase();
    if r == "docker.io" || r == "registry-1.docker.io" {
        "index.docker.io".to_string()
    } else {
        r
    }
}

impl a3s_box_core::traits::CredentialProvider for CredentialStore {
    fn get(&self, registry: &str) -> Result<Option<(String, String)>> {
        self.get(registry)
    }

    fn store(&self, registry: &str, username: &str, password: &str) -> Result<()> {
        self.store(registry, username, password)
    }

    fn remove(&self, registry: &str) -> Result<bool> {
        self.remove(registry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use tempfile::TempDir;

    fn test_store(dir: &TempDir) -> CredentialStore {
        CredentialStore::new(dir.path().join("credentials.json"))
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn with_docker_config_dir<R>(dir: &TempDir, f: impl FnOnce() -> R) -> R {
        let _guard = env_lock();
        let previous = std::env::var_os("DOCKER_CONFIG");
        std::env::set_var("DOCKER_CONFIG", dir.path());
        let result = f();
        match previous {
            Some(value) => std::env::set_var("DOCKER_CONFIG", value),
            None => std::env::remove_var("DOCKER_CONFIG"),
        }
        result
    }

    // The advisory lock is per-open-file-description, so separate
    // FileLock::acquire calls serialize even across threads in one process —
    // which lets this exercise the lost-update fix in-process.
    #[test]
    fn concurrent_logins_to_distinct_registries_no_lost_update() {
        use std::sync::Arc;
        use std::thread;

        let dir = TempDir::new().unwrap();
        let store = Arc::new(test_store(&dir));

        let n = 16;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let store = Arc::clone(&store);
                thread::spawn(move || {
                    store
                        .store(&format!("reg{i}.example.com"), "user", "pass")
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            store.list_registries().unwrap().len(),
            n,
            "every concurrent login must persist (no lost update)"
        );
    }

    #[test]
    fn test_store_and_get() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        store.store("ghcr.io", "user1", "pass1").unwrap();
        let creds = store.get("ghcr.io").unwrap();
        assert_eq!(creds, Some(("user1".to_string(), "pass1".to_string())));
    }

    #[test]
    fn test_get_nonexistent() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        let creds = store.get("ghcr.io").unwrap();
        assert_eq!(creds, None);
    }

    #[test]
    fn test_overwrite_existing() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        store.store("ghcr.io", "user1", "pass1").unwrap();
        store.store("ghcr.io", "user2", "pass2").unwrap();
        let creds = store.get("ghcr.io").unwrap();
        assert_eq!(creds, Some(("user2".to_string(), "pass2".to_string())));
    }

    #[test]
    fn test_remove() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        store.store("ghcr.io", "user1", "pass1").unwrap();
        assert!(store.remove("ghcr.io").unwrap());
        assert_eq!(store.get("ghcr.io").unwrap(), None);
    }

    #[test]
    fn test_remove_nonexistent() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        assert!(!store.remove("ghcr.io").unwrap());
    }

    #[test]
    fn test_list_registries() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        store.store("ghcr.io", "u1", "p1").unwrap();
        store.store("quay.io", "u2", "p2").unwrap();
        let registries = store.list_registries().unwrap();
        assert_eq!(registries, vec!["ghcr.io", "quay.io"]);
    }

    #[test]
    fn test_list_empty() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        let registries = store.list_registries().unwrap();
        assert!(registries.is_empty());
    }

    #[test]
    fn test_docker_io_normalization() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        store.store("docker.io", "user", "pass").unwrap();
        // All Docker Hub aliases should resolve to the same entry
        let creds = store.get("index.docker.io").unwrap();
        assert_eq!(creds, Some(("user".to_string(), "pass".to_string())));

        let creds = store.get("registry-1.docker.io").unwrap();
        assert_eq!(creds, Some(("user".to_string(), "pass".to_string())));
    }

    #[test]
    fn docker_credentials_reads_auths_for_host_port_registry() {
        use base64::Engine as _;

        let dir = TempDir::new().unwrap();
        let auth = base64::engine::general_purpose::STANDARD.encode("user:pass");
        std::fs::write(
            dir.path().join("config.json"),
            format!(
                r#"{{
  "auths": {{
    "10.12.111.133:49164": {{ "auth": "{auth}" }}
  }}
}}"#
            ),
        )
        .unwrap();

        let creds = with_docker_config_dir(&dir, || docker_credentials("10.12.111.133:49164"));
        assert_eq!(creds, Some(("user".to_string(), "pass".to_string())));
    }

    #[test]
    fn docker_credentials_matches_docker_hub_legacy_url() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{
  "auths": {
    "https://index.docker.io/v1/": {
      "username": "dock",
      "password": "secret"
    }
  }
}"#,
        )
        .unwrap();

        let creds = with_docker_config_dir(&dir, || docker_credentials("docker.io"));
        assert_eq!(creds, Some(("dock".to_string(), "secret".to_string())));
    }

    #[test]
    fn docker_credentials_uses_identity_token_as_oauth_password() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{
  "auths": {
    "registry.example.com": {
      "identitytoken": "token-value"
    }
  }
}"#,
        )
        .unwrap();

        let creds = with_docker_config_dir(&dir, || docker_credentials("registry.example.com"));
        assert_eq!(
            creds,
            Some(("oauth2accesstoken".to_string(), "token-value".to_string()))
        );
    }

    #[test]
    fn test_persistence() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credentials.json");

        // Store with one instance
        let store1 = CredentialStore::new(path.clone());
        store1.store("ghcr.io", "user", "pass").unwrap();

        // Read with a new instance
        let store2 = CredentialStore::new(path);
        let creds = store2.get("ghcr.io").unwrap();
        assert_eq!(creds, Some(("user".to_string(), "pass".to_string())));
    }

    #[test]
    fn test_multiple_registries() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        store.store("ghcr.io", "u1", "p1").unwrap();
        store.store("quay.io", "u2", "p2").unwrap();
        store.store("ecr.aws", "u3", "p3").unwrap();

        assert_eq!(
            store.get("ghcr.io").unwrap(),
            Some(("u1".to_string(), "p1".to_string()))
        );
        assert_eq!(
            store.get("quay.io").unwrap(),
            Some(("u2".to_string(), "p2".to_string()))
        );
        assert_eq!(
            store.get("ecr.aws").unwrap(),
            Some(("u3".to_string(), "p3".to_string()))
        );
    }
}
