//! Volume types for named volume management.
//!
//! Provides volume configuration and metadata for persistent
//! named volumes that can be shared across box instances.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Configuration for a named volume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeConfig {
    /// Volume name (unique identifier).
    pub name: String,

    /// Volume driver (currently only "local" is supported).
    #[serde(default = "default_driver")]
    pub driver: String,

    /// Host path where volume data is stored.
    pub mount_point: String,

    /// User-defined labels.
    #[serde(default)]
    pub labels: HashMap<String, String>,

    /// Box IDs currently using this volume.
    #[serde(default)]
    pub in_use_by: Vec<String>,

    /// Creation timestamp (RFC 3339).
    pub created_at: String,
}

fn default_driver() -> String {
    "local".to_string()
}

impl VolumeConfig {
    /// Create a new named volume.
    pub fn new(name: &str, mount_point: &str) -> Self {
        Self {
            name: name.to_string(),
            driver: "local".to_string(),
            mount_point: mount_point.to_string(),
            labels: HashMap::new(),
            in_use_by: Vec::new(),
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Mark a box as using this volume.
    pub fn attach(&mut self, box_id: &str) {
        if !self.in_use_by.contains(&box_id.to_string()) {
            self.in_use_by.push(box_id.to_string());
        }
    }

    /// Remove a box from this volume's users.
    pub fn detach(&mut self, box_id: &str) {
        self.in_use_by.retain(|id| id != box_id);
    }

    /// Check if any boxes are using this volume.
    pub fn is_in_use(&self) -> bool {
        !self.in_use_by.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_volume_config_new() {
        let vol = VolumeConfig::new("mydata", "/home/user/.a3s/volumes/mydata");
        assert_eq!(vol.name, "mydata");
        assert_eq!(vol.driver, "local");
        assert!(vol.in_use_by.is_empty());
        assert!(vol.labels.is_empty());
    }

    #[test]
    fn test_volume_attach_detach() {
        let mut vol = VolumeConfig::new("mydata", "/tmp/vol");
        vol.attach("box-1");
        vol.attach("box-2");
        assert_eq!(vol.in_use_by.len(), 2);
        assert!(vol.is_in_use());

        vol.detach("box-1");
        assert_eq!(vol.in_use_by.len(), 1);
        assert!(vol.is_in_use());

        vol.detach("box-2");
        assert!(!vol.is_in_use());
    }

    #[test]
    fn test_volume_attach_idempotent() {
        let mut vol = VolumeConfig::new("mydata", "/tmp/vol");
        vol.attach("box-1");
        vol.attach("box-1");
        assert_eq!(vol.in_use_by.len(), 1);
    }

    #[test]
    fn test_volume_detach_nonexistent() {
        let mut vol = VolumeConfig::new("mydata", "/tmp/vol");
        vol.detach("nonexistent"); // should not panic
        assert!(vol.in_use_by.is_empty());
    }

    #[test]
    fn test_volume_with_labels() {
        let mut vol = VolumeConfig::new("mydata", "/tmp/vol");
        vol.labels.insert("env".to_string(), "prod".to_string());
        assert_eq!(vol.labels.get("env").unwrap(), "prod");
    }

    #[test]
    fn test_volume_serialization() {
        let mut vol = VolumeConfig::new("mydata", "/tmp/vol");
        vol.attach("box-1");
        vol.labels.insert("env".to_string(), "test".to_string());

        let json = serde_json::to_string(&vol).unwrap();
        let parsed: VolumeConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.name, "mydata");
        assert_eq!(parsed.in_use_by, vec!["box-1"]);
        assert_eq!(parsed.labels.get("env").unwrap(), "test");
    }

    #[test]
    fn test_volume_default_driver() {
        let json = r#"{"name":"test","mount_point":"/tmp","created_at":"2024-01-01T00:00:00Z"}"#;
        let vol: VolumeConfig = serde_json::from_str(json).unwrap();
        assert_eq!(vol.driver, "local");
    }
}
