//! Skill system with "mount = activated" pattern
//!
//! The runtime manages skill discovery and filtering. It provides skill content
//! to the code agent, which handles actual tool parsing and execution.
//!
//! ## Architecture
//!
//! ```text
//! Runtime (host)              Code Agent (guest)
//! ─────────────────           ──────────────────
//! SkillManager                ToolExecutor
//!   ├── scan()                  └── register_skill_tools(content)
//!   ├── list(&filter)                 ├── parse_skill_tools()
//!   └── get()                         └── Tool implementations
//!         │
//!         └──── content ────────────────►
//! ```

use a3s_box_core::error::{BoxError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Skill filter trait - controls which skills to include
///
/// Implement this trait to filter skills and prevent context explosion.
///
/// # Example
///
/// ```ignore
/// struct SessionFilter {
///     allowed: HashSet<String>,
/// }
///
/// impl SkillFilter for SessionFilter {
///     fn filter(&self, skill: &SkillPackage) -> bool {
///         self.allowed.contains(&skill.name)
///     }
/// }
/// ```
pub trait SkillFilter: Send + Sync {
    /// Return true to include the skill, false to exclude
    fn filter(&self, skill: &SkillPackage) -> bool;
}

/// Default filter - includes all skills (use with caution)
#[derive(Debug, Clone, Default)]
pub struct NoFilter;

impl SkillFilter for NoFilter {
    fn filter(&self, _skill: &SkillPackage) -> bool {
        true
    }
}

/// Name-based filter
#[derive(Debug, Clone)]
pub struct NameFilter {
    allowed: Vec<String>,
}

impl NameFilter {
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed }
    }
}

impl SkillFilter for NameFilter {
    fn filter(&self, skill: &SkillPackage) -> bool {
        self.allowed.contains(&skill.name)
    }
}

/// Limit filter - caps the number of skills
#[derive(Debug)]
pub struct LimitFilter {
    max: usize,
    count: std::sync::atomic::AtomicUsize,
}

impl Clone for LimitFilter {
    fn clone(&self) -> Self {
        Self {
            max: self.max,
            count: std::sync::atomic::AtomicUsize::new(
                self.count.load(std::sync::atomic::Ordering::SeqCst),
            ),
        }
    }
}

impl LimitFilter {
    pub fn new(max: usize) -> Self {
        Self {
            max,
            count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn reset(&self) {
        self.count.store(0, std::sync::atomic::Ordering::SeqCst);
    }
}

impl SkillFilter for LimitFilter {
    fn filter(&self, _skill: &SkillPackage) -> bool {
        let current = self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        current < self.max
    }
}

/// Skill package - metadata and content for a skill
///
/// The runtime only needs to know basic metadata and provide content
/// to the code agent. Tool parsing happens in the code agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillPackage {
    /// Skill name (from frontmatter)
    pub name: String,

    /// Description (from frontmatter)
    pub description: String,

    /// Version (from frontmatter, optional)
    pub version: Option<String>,

    /// Lane for queue routing (from frontmatter, optional)
    pub lane: Option<String>,

    /// Full SKILL.md content - passed to code agent for tool registration
    pub content: String,

    /// Source file path
    pub path: PathBuf,
}

/// Skill manager - discovers and provides access to mounted skills
pub struct SkillManager {
    /// Skill directories to scan
    skill_dirs: Vec<PathBuf>,

    /// Discovered packages
    packages: Arc<RwLock<HashMap<String, SkillPackage>>>,
}

impl SkillManager {
    /// Create a new skill manager
    pub fn new(skill_dirs: Vec<PathBuf>) -> Self {
        Self {
            skill_dirs,
            packages: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Scan skill directories (mount = activated)
    ///
    /// Returns the number of skills discovered.
    pub async fn scan(&self) -> Result<usize> {
        let mut packages = self.packages.write().await;
        packages.clear();

        for dir in &self.skill_dirs {
            if !dir.exists() {
                continue;
            }
            self.scan_dir(dir, &mut packages).await?;
        }

        Ok(packages.len())
    }

    /// Scan a single directory
    async fn scan_dir(
        &self,
        dir: &Path,
        packages: &mut HashMap<String, SkillPackage>,
    ) -> Result<()> {
        let entries = std::fs::read_dir(dir).map_err(|e| {
            BoxError::SkillError(format!("Failed to read {}: {}", dir.display(), e))
        })?;

        for entry in entries.flatten() {
            let path = entry.path();

            // Skill in subdirectory: skills/my-skill/SKILL.md
            if path.is_dir() {
                let skill_md = path.join("SKILL.md");
                if skill_md.exists() {
                    if let Ok(pkg) = self.parse(&skill_md).await {
                        packages.insert(pkg.name.clone(), pkg);
                    }
                }
            }
            // Standalone skill file: skills/my-skill.md (legacy)
            else if path.extension().and_then(|s| s.to_str()) == Some("md") {
                if let Ok(pkg) = self.parse(&path).await {
                    packages.insert(pkg.name.clone(), pkg);
                }
            }
        }

        Ok(())
    }

    /// Parse a SKILL.md file
    async fn parse(&self, path: &Path) -> Result<SkillPackage> {
        let content = tokio::fs::read_to_string(path).await.map_err(|e| {
            BoxError::SkillError(format!("Failed to read {}: {}", path.display(), e))
        })?;

        let frontmatter = parse_frontmatter(&content)?;

        let name = frontmatter
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| BoxError::SkillError(format!("Missing 'name' in {}", path.display())))?
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

        Ok(SkillPackage {
            name,
            description,
            version,
            lane,
            content, // Full content for code agent
            path: path.to_path_buf(),
        })
    }

    /// Get a skill by name
    pub async fn get(&self, name: &str) -> Result<SkillPackage> {
        let packages = self.packages.read().await;
        packages
            .get(name)
            .cloned()
            .ok_or_else(|| BoxError::SkillError(format!("Skill not found: {}", name)))
    }

    /// List all skills (unfiltered)
    pub async fn list_all(&self) -> Vec<SkillPackage> {
        let packages = self.packages.read().await;
        packages.values().cloned().collect()
    }

    /// List skills with filter
    pub async fn list<F: SkillFilter>(&self, filter: &F) -> Vec<SkillPackage> {
        let packages = self.packages.read().await;
        packages
            .values()
            .filter(|p| filter.filter(p))
            .cloned()
            .collect()
    }

    /// List skill names only (lightweight)
    pub async fn list_names(&self) -> Vec<String> {
        let packages = self.packages.read().await;
        packages.keys().cloned().collect()
    }

    /// Get skill content for code agent registration
    ///
    /// Use this to pass skill content to `ToolExecutor.register_skill_tools()`.
    pub async fn get_content(&self, name: &str) -> Result<String> {
        let pkg = self.get(name).await?;
        Ok(pkg.content)
    }

    /// Get all skill contents with filter
    ///
    /// Returns (name, content) pairs for registration with code agent.
    pub async fn get_contents<F: SkillFilter>(&self, filter: &F) -> Vec<(String, String)> {
        let packages = self.packages.read().await;
        packages
            .values()
            .filter(|p| filter.filter(p))
            .map(|p| (p.name.clone(), p.content.clone()))
            .collect()
    }
}

/// Parse YAML frontmatter from markdown
fn parse_frontmatter(content: &str) -> Result<serde_yaml::Value> {
    let parts: Vec<&str> = content.splitn(3, "---").collect();

    if parts.len() < 3 {
        return Ok(serde_yaml::Value::Null);
    }

    serde_yaml::from_str(parts[1])
        .map_err(|e| BoxError::SkillError(format!("Failed to parse frontmatter: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_package(name: &str) -> SkillPackage {
        SkillPackage {
            name: name.to_string(),
            description: String::new(),
            version: None,
            lane: None,
            content: String::new(),
            path: PathBuf::new(),
        }
    }

    // ===================
    // Filter Tests
    // ===================

    #[test]
    fn test_no_filter() {
        let filter = NoFilter;
        assert!(filter.filter(&make_package("any-skill")));
        assert!(filter.filter(&make_package("another-skill")));
    }

    #[test]
    fn test_name_filter() {
        let filter = NameFilter::new(vec!["web-search".to_string(), "image-gen".to_string()]);

        assert!(filter.filter(&make_package("web-search")));
        assert!(filter.filter(&make_package("image-gen")));
        assert!(!filter.filter(&make_package("other")));
        assert!(!filter.filter(&make_package("")));
    }

    #[test]
    fn test_name_filter_empty() {
        let filter = NameFilter::new(vec![]);
        assert!(!filter.filter(&make_package("any-skill")));
    }

    #[test]
    fn test_limit_filter() {
        let filter = LimitFilter::new(2);

        assert!(filter.filter(&make_package("a"))); // 1
        assert!(filter.filter(&make_package("b"))); // 2
        assert!(!filter.filter(&make_package("c"))); // 3 - exceeds
        assert!(!filter.filter(&make_package("d"))); // 4 - still exceeds

        filter.reset();
        assert!(filter.filter(&make_package("e"))); // Reset works
    }

    #[test]
    fn test_limit_filter_zero() {
        let filter = LimitFilter::new(0);
        assert!(!filter.filter(&make_package("any")));
    }

    // ===================
    // Frontmatter Tests
    // ===================

    #[test]
    fn test_parse_frontmatter_full() {
        let content = r#"---
name: test-skill
description: A test skill
version: "1.0.0"
lane: skill
tools:
  - name: my-tool
    description: Does something
---

# Test Skill

Instructions for the skill.
"#;

        let fm = parse_frontmatter(content).unwrap();
        assert_eq!(fm.get("name").unwrap().as_str().unwrap(), "test-skill");
        assert_eq!(
            fm.get("description").unwrap().as_str().unwrap(),
            "A test skill"
        );
        assert_eq!(fm.get("version").unwrap().as_str().unwrap(), "1.0.0");
        assert_eq!(fm.get("lane").unwrap().as_str().unwrap(), "skill");
        assert!(fm.get("tools").unwrap().as_sequence().is_some());
    }

    #[test]
    fn test_parse_frontmatter_minimal() {
        let content = r#"---
name: minimal
---
Content only.
"#;

        let fm = parse_frontmatter(content).unwrap();
        assert_eq!(fm.get("name").unwrap().as_str().unwrap(), "minimal");
        assert!(fm.get("description").is_none());
    }

    #[test]
    fn test_parse_frontmatter_no_markers() {
        let content = "Just plain markdown without frontmatter.";
        let fm = parse_frontmatter(content).unwrap();
        assert!(fm.is_null());
    }

    #[test]
    fn test_parse_frontmatter_invalid_yaml() {
        let content = r#"---
name: [invalid yaml
---
Content.
"#;

        let result = parse_frontmatter(content);
        assert!(result.is_err());
    }

    // ===================
    // SkillManager Tests
    // ===================

    #[tokio::test]
    async fn test_skill_manager_empty_dirs() {
        let manager = SkillManager::new(vec![]);
        let count = manager.scan().await.unwrap();
        assert_eq!(count, 0);
        assert!(manager.list_all().await.is_empty());
    }

    #[tokio::test]
    async fn test_skill_manager_nonexistent_dir() {
        let manager = SkillManager::new(vec![PathBuf::from("/nonexistent/path")]);
        let count = manager.scan().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_skill_manager_scan_subdir() {
        let temp = TempDir::new().unwrap();

        // Create skill in subdirectory: skills/my-skill/SKILL.md
        let skill_dir = temp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: my-skill
description: Test skill in subdirectory
version: "1.0"
---
# My Skill
"#,
        )
        .unwrap();

        let manager = SkillManager::new(vec![temp.path().to_path_buf()]);
        let count = manager.scan().await.unwrap();

        assert_eq!(count, 1);

        let pkg = manager.get("my-skill").await.unwrap();
        assert_eq!(pkg.name, "my-skill");
        assert_eq!(pkg.description, "Test skill in subdirectory");
        assert_eq!(pkg.version, Some("1.0".to_string()));
    }

    #[tokio::test]
    async fn test_skill_manager_scan_standalone() {
        let temp = TempDir::new().unwrap();

        // Create standalone skill file: skills/standalone.md (legacy format)
        std::fs::write(
            temp.path().join("standalone.md"),
            r#"---
name: standalone-skill
description: Legacy standalone skill
---
# Standalone
"#,
        )
        .unwrap();

        let manager = SkillManager::new(vec![temp.path().to_path_buf()]);
        let count = manager.scan().await.unwrap();

        assert_eq!(count, 1);

        let pkg = manager.get("standalone-skill").await.unwrap();
        assert_eq!(pkg.name, "standalone-skill");
    }

    #[tokio::test]
    async fn test_skill_manager_scan_multiple() {
        let temp = TempDir::new().unwrap();

        // Create multiple skills
        for i in 1..=3 {
            let skill_dir = temp.path().join(format!("skill-{}", i));
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!(
                    r#"---
name: skill-{}
description: Skill number {}
---
"#,
                    i, i
                ),
            )
            .unwrap();
        }

        let manager = SkillManager::new(vec![temp.path().to_path_buf()]);
        let count = manager.scan().await.unwrap();

        assert_eq!(count, 3);
        assert_eq!(manager.list_names().await.len(), 3);
    }

    #[tokio::test]
    async fn test_skill_manager_list_with_filter() {
        let temp = TempDir::new().unwrap();

        // Create skills
        for name in ["web-search", "image-gen", "code-exec"] {
            let skill_dir = temp.path().join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: {}\n---\n", name),
            )
            .unwrap();
        }

        let manager = SkillManager::new(vec![temp.path().to_path_buf()]);
        manager.scan().await.unwrap();

        // Filter by name
        let filter = NameFilter::new(vec!["web-search".to_string(), "image-gen".to_string()]);
        let filtered = manager.list(&filter).await;

        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().any(|p| p.name == "web-search"));
        assert!(filtered.iter().any(|p| p.name == "image-gen"));
        assert!(!filtered.iter().any(|p| p.name == "code-exec"));
    }

    #[tokio::test]
    async fn test_skill_manager_get_content() {
        let temp = TempDir::new().unwrap();

        let skill_dir = temp.path().join("test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let content = r#"---
name: test-skill
tools:
  - name: my-tool
    backend:
      type: script
      interpreter: bash
      script: echo "hello"
---
# Test Skill

This is the skill content for LLM context.
"#;
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();

        let manager = SkillManager::new(vec![temp.path().to_path_buf()]);
        manager.scan().await.unwrap();

        let retrieved = manager.get_content("test-skill").await.unwrap();
        assert_eq!(retrieved, content);
    }

    #[tokio::test]
    async fn test_skill_manager_get_contents_with_filter() {
        let temp = TempDir::new().unwrap();

        for name in ["a", "b", "c"] {
            let skill_dir = temp.path().join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: {}\n---\nContent for {}", name, name),
            )
            .unwrap();
        }

        let manager = SkillManager::new(vec![temp.path().to_path_buf()]);
        manager.scan().await.unwrap();

        let filter = LimitFilter::new(2);
        let contents = manager.get_contents(&filter).await;

        assert_eq!(contents.len(), 2);
    }

    #[tokio::test]
    async fn test_skill_manager_get_not_found() {
        let manager = SkillManager::new(vec![]);
        manager.scan().await.unwrap();

        let result = manager.get("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_skill_manager_rescan_clears_old() {
        let temp = TempDir::new().unwrap();

        // Create initial skill
        let skill_dir = temp.path().join("old-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "---\nname: old-skill\n---\n").unwrap();

        let manager = SkillManager::new(vec![temp.path().to_path_buf()]);
        manager.scan().await.unwrap();
        assert_eq!(manager.list_names().await.len(), 1);

        // Remove old, add new
        std::fs::remove_dir_all(&skill_dir).unwrap();
        let new_skill_dir = temp.path().join("new-skill");
        std::fs::create_dir_all(&new_skill_dir).unwrap();
        std::fs::write(
            new_skill_dir.join("SKILL.md"),
            "---\nname: new-skill\n---\n",
        )
        .unwrap();

        // Rescan
        manager.scan().await.unwrap();

        let names = manager.list_names().await;
        assert_eq!(names.len(), 1);
        assert!(names.contains(&"new-skill".to_string()));
        assert!(!names.contains(&"old-skill".to_string()));
    }
}
