//! Skill tool loader
//!
//! Converts skill tool definitions to dynamic Tool implementations.

use super::dynamic::{BinaryTool, HttpTool, ScriptTool};
use super::types::ToolBackend;
use super::Tool;
use std::sync::Arc;

/// Skill tool definition (extended from runtime's SkillTool)
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SkillToolDef {
    /// Tool name
    pub name: String,

    /// Tool description
    #[serde(default)]
    pub description: String,

    /// JSON Schema for parameters
    #[serde(default = "default_parameters")]
    pub parameters: serde_json::Value,

    /// Backend configuration
    #[serde(default)]
    pub backend: ToolBackend,

    // Legacy fields for backward compatibility
    /// Remote URL (legacy, maps to Binary backend)
    #[serde(default)]
    pub url: Option<String>,

    /// Local binary path (legacy, maps to Binary backend)
    #[serde(default)]
    pub bin: Option<String>,
}

fn default_parameters() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "required": []
    })
}

impl SkillToolDef {
    /// Convert to a Tool implementation
    pub fn into_tool(self) -> Arc<dyn Tool> {
        // Determine backend from explicit backend field or legacy fields
        let backend = self.resolve_backend();

        match backend {
            ToolBackend::Builtin => {
                // Builtin tools are already registered, this shouldn't happen
                // Return a no-op tool that errors
                Arc::new(ScriptTool::new(
                    self.name,
                    self.description,
                    self.parameters,
                    "bash".to_string(),
                    "echo 'Error: builtin tools cannot be loaded from skills'".to_string(),
                    vec![],
                ))
            }
            ToolBackend::Binary {
                url,
                path,
                args_template,
            } => Arc::new(BinaryTool::new(
                self.name,
                self.description,
                self.parameters,
                url,
                path,
                args_template,
            )),
            ToolBackend::Http {
                url,
                method,
                headers,
                body_template,
                timeout_ms,
            } => Arc::new(HttpTool::new(
                self.name,
                self.description,
                self.parameters,
                url,
                method,
                headers,
                body_template,
                timeout_ms,
            )),
            ToolBackend::Script {
                interpreter,
                script,
                interpreter_args,
            } => Arc::new(ScriptTool::new(
                self.name,
                self.description,
                self.parameters,
                interpreter,
                script,
                interpreter_args,
            )),
        }
    }

    /// Resolve backend from explicit field or legacy fields
    fn resolve_backend(&self) -> ToolBackend {
        // If explicit backend is set and not Builtin, use it
        match &self.backend {
            ToolBackend::Builtin => {
                // Check legacy fields
                if let Some(url) = &self.url {
                    ToolBackend::Binary {
                        url: Some(url.clone()),
                        path: self.bin.clone(),
                        args_template: None,
                    }
                } else if let Some(bin) = &self.bin {
                    ToolBackend::Binary {
                        url: None,
                        path: Some(bin.clone()),
                        args_template: None,
                    }
                } else {
                    // Default to script with empty bash
                    ToolBackend::Script {
                        interpreter: "bash".to_string(),
                        script: "echo 'No backend configured'".to_string(),
                        interpreter_args: vec![],
                    }
                }
            }
            other => other.clone(),
        }
    }
}

/// Load tools from a skill definition
///
/// Takes the tools array from a SKILL.md frontmatter and returns
/// Tool implementations.
pub fn load_tools_from_skill(tools_yaml: &serde_yaml::Value) -> Vec<Arc<dyn Tool>> {
    let Some(tools_array) = tools_yaml.as_sequence() else {
        return vec![];
    };

    tools_array
        .iter()
        .filter_map(|tool_yaml| {
            // Convert YAML to JSON for easier handling
            let json_str = serde_json::to_string(tool_yaml).ok()?;
            let tool_def: SkillToolDef = serde_json::from_str(&json_str).ok()?;
            Some(tool_def.into_tool())
        })
        .collect()
}

/// Parse skill frontmatter and extract tools
pub fn parse_skill_tools(content: &str) -> Vec<Arc<dyn Tool>> {
    // Parse frontmatter (YAML between --- markers)
    let parts: Vec<&str> = content.splitn(3, "---").collect();

    if parts.len() < 3 {
        return vec![];
    }

    let frontmatter: serde_yaml::Value = match serde_yaml::from_str(parts[1]) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let tools_yaml = frontmatter.get("tools").cloned().unwrap_or(serde_yaml::Value::Null);
    load_tools_from_skill(&tools_yaml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_skill_tools_binary() {
        let content = r#"---
name: test-skill
description: A test skill
tools:
  - name: my-tool
    description: A binary tool
    backend:
      type: binary
      path: /usr/bin/echo
      args_template: "${message}"
    parameters:
      type: object
      properties:
        message:
          type: string
      required:
        - message
---
# Test Skill

This is a test skill.
"#;

        let tools = parse_skill_tools(content);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "my-tool");
    }

    #[test]
    fn test_parse_skill_tools_http() {
        let content = r#"---
name: api-skill
tools:
  - name: api-call
    description: Make an API call
    backend:
      type: http
      url: https://api.example.com/endpoint
      method: POST
      headers:
        Authorization: "Bearer ${env:API_KEY}"
    parameters:
      type: object
      properties:
        query:
          type: string
---
"#;

        let tools = parse_skill_tools(content);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "api-call");
    }

    #[test]
    fn test_parse_skill_tools_script() {
        let content = r#"---
name: script-skill
tools:
  - name: process-data
    description: Process data with Python
    backend:
      type: script
      interpreter: python3
      script: |
        import os
        import json
        args = json.loads(os.environ.get('TOOL_ARGS', '{}'))
        print(f"Processing: {args}")
    parameters:
      type: object
      properties:
        data:
          type: string
---
"#;

        let tools = parse_skill_tools(content);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "process-data");
    }

    #[test]
    fn test_parse_skill_tools_legacy() {
        let content = r#"---
name: legacy-skill
tools:
  - name: legacy-tool
    description: A legacy tool with url field
    url: https://tools.example.com/my-tool
---
"#;

        let tools = parse_skill_tools(content);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "legacy-tool");
    }

    #[test]
    fn test_parse_skill_tools_empty() {
        let content = r#"---
name: empty-skill
---
No tools here.
"#;

        let tools = parse_skill_tools(content);
        assert!(tools.is_empty());
    }

    #[test]
    fn test_skill_tool_def_resolve_backend() {
        // Test legacy url field
        let def = SkillToolDef {
            name: "test".to_string(),
            description: "test".to_string(),
            parameters: serde_json::json!({}),
            backend: ToolBackend::Builtin,
            url: Some("https://example.com/tool".to_string()),
            bin: None,
        };

        let backend = def.resolve_backend();
        match backend {
            ToolBackend::Binary { url, .. } => {
                assert_eq!(url, Some("https://example.com/tool".to_string()));
            }
            _ => panic!("Expected Binary backend"),
        }
    }
}
