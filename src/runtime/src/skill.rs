use a3s_box_core::error::{BoxError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Skill package metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillPackage {
    /// Skill name
    pub name: String,

    /// Description
    pub description: String,

    /// Version
    pub version: Option<String>,

    /// Lane (for dynamic skill lanes)
    pub lane: Option<String>,

    /// Tools
    pub tools: Vec<SkillTool>,

    /// Full SKILL.md content
    pub content: String,

    /// File path
    pub path: PathBuf,
}

/// Skill tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillTool {
    /// Tool name
    pub name: String,

    /// Remote URL (for download)
    pub url: Option<String>,

    /// Local binary path (for development)
    pub bin: Option<PathBuf>,

    /// Description
    pub description: String,
}

/// Skill activation state
#[derive(Debug, Clone)]
pub struct Skill {
    /// Skill package
    pub package: SkillPackage,

    /// Cached tool paths
    pub cached_tools: HashMap<String, PathBuf>,

    /// Activation timestamp
    pub activated_at: chrono::DateTime<chrono::Utc>,
}

/// Skill manager
pub struct SkillManager {
    /// Skill directories
    skill_dirs: Vec<PathBuf>,

    /// Discovered skill packages (metadata only)
    packages: Arc<RwLock<HashMap<String, SkillPackage>>>,

    /// Cache directory
    cache_dir: PathBuf,
}

impl SkillManager {
    /// Create a new skill manager
    pub fn new(skill_dirs: Vec<PathBuf>, cache_dir: PathBuf) -> Self {
        Self {
            skill_dirs,
            packages: Arc::new(RwLock::new(HashMap::new())),
            cache_dir,
        }
    }

    /// Scan skill directories for SKILL.md files
    pub async fn scan_skills(&self) -> Result<()> {
        let mut packages = self.packages.write().await;

        for dir in &self.skill_dirs {
            if !dir.exists() {
                continue;
            }

            let entries = std::fs::read_dir(dir).map_err(|e| {
                BoxError::SkillError(format!("Failed to read skill directory: {}", e))
            })?;

            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("md") {
                    if let Ok(package) = self.parse_skill_package(&path).await {
                        packages.insert(package.name.clone(), package);
                    }
                }
            }
        }

        Ok(())
    }

    /// Parse a SKILL.md file
    async fn parse_skill_package(&self, path: &Path) -> Result<SkillPackage> {
        let content = tokio::fs::read_to_string(path).await.map_err(|e| {
            BoxError::SkillError(format!("Failed to read skill file: {}", e))
        })?;

        // Parse frontmatter (YAML between --- markers)
        let (frontmatter, body) = self.parse_frontmatter(&content)?;

        let name = frontmatter
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| BoxError::SkillError("Skill name not found".to_string()))?
            .to_string();

        let description = frontmatter
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let version = frontmatter
            .get("version")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let lane = frontmatter
            .get("lane")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let tools = frontmatter
            .get("tools")
            .and_then(|v| v.as_sequence())
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| {
                        let name = tool.get("name")?.as_str()?.to_string();
                        let url = tool.get("url").and_then(|v| v.as_str()).map(|s| s.to_string());
                        let bin = tool.get("bin").and_then(|v| v.as_str()).map(PathBuf::from);
                        let description = tool
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        Some(SkillTool {
                            name,
                            url,
                            bin,
                            description,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(SkillPackage {
            name,
            description,
            version,
            lane,
            tools,
            content: body,
            path: path.to_path_buf(),
        })
    }

    /// Parse frontmatter from SKILL.md content
    fn parse_frontmatter(&self, content: &str) -> Result<(serde_yaml::Value, String)> {
        let parts: Vec<&str> = content.splitn(3, "---").collect();

        if parts.len() < 3 {
            return Ok((serde_yaml::Value::Null, content.to_string()));
        }

        let frontmatter = serde_yaml::from_str(parts[1]).map_err(|e| {
            BoxError::SkillError(format!("Failed to parse frontmatter: {}", e))
        })?;

        let body = parts[2].trim().to_string();

        Ok((frontmatter, body))
    }

    /// Get skill package by name
    pub async fn get_package(&self, name: &str) -> Result<SkillPackage> {
        let packages = self.packages.read().await;
        packages
            .get(name)
            .cloned()
            .ok_or_else(|| BoxError::SkillError(format!("Skill not found: {}", name)))
    }

    /// List all available skills (metadata only)
    pub async fn list_packages(&self) -> Vec<SkillPackage> {
        let packages = self.packages.read().await;
        packages.values().cloned().collect()
    }

    /// Activate a skill (download tools, create symlinks)
    pub async fn activate_skill(&self, name: &str) -> Result<Skill> {
        let package = self.get_package(name).await?;
        let mut cached_tools = HashMap::new();

        for tool in &package.tools {
            let cached_path = if let Some(url) = &tool.url {
                // Download from URL
                self.download_tool(&package.name, &tool.name, url).await?
            } else if let Some(bin) = &tool.bin {
                // Use local binary
                bin.clone()
            } else {
                continue;
            };

            cached_tools.insert(tool.name.clone(), cached_path);
        }

        Ok(Skill {
            package,
            cached_tools,
            activated_at: chrono::Utc::now(),
        })
    }

    /// Download a tool from URL
    async fn download_tool(&self, skill_name: &str, tool_name: &str, url: &str) -> Result<PathBuf> {
        // Resolve platform-specific URL
        let resolved_url = self.resolve_tool_url(url);

        // Check cache
        let cache_path = self.cache_dir
            .join(skill_name)
            .join(tool_name);

        if cache_path.exists() {
            return Ok(cache_path);
        }

        // Download
        let response = reqwest::get(&resolved_url).await.map_err(|e| {
            BoxError::ToolDownloadError {
                url: resolved_url.clone(),
                status_code: 0,
                message: e.to_string(),
            }
        })?;

        if !response.status().is_success() {
            return Err(BoxError::ToolDownloadError {
                url: resolved_url,
                status_code: response.status().as_u16(),
                message: "Download failed".to_string(),
            });
        }

        let bytes = response.bytes().await.map_err(|e| {
            BoxError::ToolDownloadError {
                url: resolved_url,
                status_code: 0,
                message: e.to_string(),
            }
        })?;

        // Save to cache
        tokio::fs::create_dir_all(cache_path.parent().unwrap()).await?;
        tokio::fs::write(&cache_path, bytes).await?;

        // Make executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = tokio::fs::metadata(&cache_path).await?.permissions();
            perms.set_mode(0o755);
            tokio::fs::set_permissions(&cache_path, perms).await?;
        }

        Ok(cache_path)
    }

    /// Resolve platform-specific tool URL
    fn resolve_tool_url(&self, base_url: &str) -> String {
        let arch = if cfg!(target_arch = "x86_64") {
            "amd64"
        } else if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            return base_url.to_string();
        };

        format!("{}-linux-{}", base_url, arch)
    }
}
