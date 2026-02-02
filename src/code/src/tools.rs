//! Tool execution implementations
//!
//! Provides sandboxed tool implementations for the coding agent:
//! - bash: Execute shell commands
//! - read: Read files with optional line range
//! - write: Write content to files
//! - edit: Edit files with string replacement
//! - grep: Search file contents with regex
//! - glob: Find files matching patterns

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Maximum output size in bytes before truncation
const MAX_OUTPUT_SIZE: usize = 100 * 1024; // 100KB

/// Maximum lines to read from a file
const MAX_READ_LINES: usize = 2000;

/// Maximum line length before truncation
const MAX_LINE_LENGTH: usize = 2000;

/// Tool execution result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub name: String,
    pub output: String,
    pub exit_code: i32,
}

impl ToolResult {
    pub fn success(name: &str, output: String) -> Self {
        Self {
            name: name.to_string(),
            output,
            exit_code: 0,
        }
    }

    pub fn error(name: &str, message: String) -> Self {
        Self {
            name: name.to_string(),
            output: message,
            exit_code: 1,
        }
    }
}

/// Tool executor with workspace sandboxing
pub struct ToolExecutor {
    workspace: PathBuf,
}

impl ToolExecutor {
    pub fn new(workspace: String) -> Self {
        let workspace_path = PathBuf::from(&workspace);
        tracing::info!("ToolExecutor initialized with workspace: {}", workspace_path.display());
        Self {
            workspace: workspace_path,
        }
    }

    /// Execute a tool by name
    pub async fn execute(&self, name: &str, args: &serde_json::Value) -> Result<ToolResult> {
        tracing::info!("Executing tool: {} with args: {}", name, args);

        let result = match name {
            "bash" => self.execute_bash(args).await,
            "read" => self.execute_read(args).await,
            "write" => self.execute_write(args).await,
            "edit" => self.execute_edit(args).await,
            "grep" => self.execute_grep(args).await,
            "glob" => self.execute_glob(args).await,
            "ls" => self.execute_ls(args).await,
            _ => Ok(ToolResult::error(name, format!("Unknown tool: {}", name))),
        };

        match &result {
            Ok(r) => tracing::info!("Tool {} completed with exit_code={}", name, r.exit_code),
            Err(e) => tracing::error!("Tool {} failed: {}", name, e),
        }

        result
    }

    /// Resolve path relative to workspace, ensuring it stays within sandbox
    fn resolve_path(&self, path: &str) -> Result<PathBuf> {
        let path = Path::new(path);

        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.join(path)
        };

        // Canonicalize to resolve .. and symlinks
        let canonical = resolved
            .canonicalize()
            .unwrap_or_else(|_| resolved.clone());

        // Security check: ensure path is within workspace
        if !canonical.starts_with(&self.workspace) {
            anyhow::bail!(
                "Path {} is outside workspace {}",
                canonical.display(),
                self.workspace.display()
            );
        }

        Ok(resolved)
    }

    /// Execute bash command
    async fn execute_bash(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let command = args["command"]
            .as_str()
            .context("Missing 'command' parameter")?;

        let timeout_ms = args["timeout"].as_u64().unwrap_or(120_000);

        tracing::debug!("Bash command: {}", command);

        let mut child = Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(&self.workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn bash process")?;

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let mut stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        let mut output = String::new();
        let mut total_size = 0usize;
        let mut truncated = false;

        // Read output with timeout
        let timeout = tokio::time::Duration::from_millis(timeout_ms);
        let result = tokio::time::timeout(timeout, async {
            loop {
                tokio::select! {
                    line = stdout_reader.next_line() => {
                        match line {
                            Ok(Some(line)) => {
                                if total_size < MAX_OUTPUT_SIZE {
                                    output.push_str(&line);
                                    output.push('\n');
                                    total_size += line.len() + 1;
                                } else {
                                    truncated = true;
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                tracing::warn!("Error reading stdout: {}", e);
                                break;
                            }
                        }
                    }
                    line = stderr_reader.next_line() => {
                        match line {
                            Ok(Some(line)) => {
                                if total_size < MAX_OUTPUT_SIZE {
                                    output.push_str(&line);
                                    output.push('\n');
                                    total_size += line.len() + 1;
                                } else {
                                    truncated = true;
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                tracing::warn!("Error reading stderr: {}", e);
                            }
                        }
                    }
                }
            }
        })
        .await;

        // Handle timeout
        if result.is_err() {
            child.kill().await.ok();
            return Ok(ToolResult {
                name: "bash".to_string(),
                output: format!(
                    "{}\n\n[Command timed out after {}ms]",
                    output, timeout_ms
                ),
                exit_code: 124, // Standard timeout exit code
            });
        }

        let status = child.wait().await.context("Failed to wait for process")?;
        let exit_code = status.code().unwrap_or(-1);

        if truncated {
            output.push_str(&format!(
                "\n\n[Output truncated at {} bytes]",
                MAX_OUTPUT_SIZE
            ));
        }

        Ok(ToolResult {
            name: "bash".to_string(),
            output,
            exit_code,
        })
    }

    /// Read file contents
    async fn execute_read(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let file_path = args["file_path"]
            .as_str()
            .context("Missing 'file_path' parameter")?;

        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().unwrap_or(MAX_READ_LINES as u64) as usize;

        tracing::info!("Reading file: {} (workspace: {})", file_path, self.workspace.display());

        let resolved_path = self.resolve_path(file_path)?;

        tracing::debug!("Resolved absolute path: {}", resolved_path.display());

        // Check if file exists
        if !resolved_path.exists() {
            return Ok(ToolResult::error(
                "read",
                format!("File not found: {}", file_path),
            ));
        }

        // Check if it's a directory
        if resolved_path.is_dir() {
            return Ok(ToolResult::error(
                "read",
                format!("{} is a directory, not a file", file_path),
            ));
        }

        // Check if it's an image
        if is_image_file(&resolved_path) {
            return self.read_image(&resolved_path).await;
        }

        // Read text file
        let content = tokio::fs::read_to_string(&resolved_path)
            .await
            .with_context(|| format!("Failed to read file: {}", file_path))?;

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // Apply offset and limit
        let start = offset.min(total_lines);
        let end = (start + limit).min(total_lines);
        let selected_lines = &lines[start..end];

        // Format with line numbers (1-indexed like cat -n)
        let mut output = String::new();
        for (i, line) in selected_lines.iter().enumerate() {
            let line_num = start + i + 1;
            let truncated_line = if line.len() > MAX_LINE_LENGTH {
                format!("{}...", &line[..MAX_LINE_LENGTH])
            } else {
                line.to_string()
            };
            output.push_str(&format!("{:>6}\t{}\n", line_num, truncated_line));
        }

        // Add metadata if truncated
        if end < total_lines {
            output.push_str(&format!(
                "\n[Showing lines {}-{} of {}. Use offset/limit for more.]",
                start + 1,
                end,
                total_lines
            ));
        }

        Ok(ToolResult::success("read", output))
    }

    /// Read image file and return base64
    async fn read_image(&self, path: &Path) -> Result<ToolResult> {
        let content = tokio::fs::read(path)
            .await
            .with_context(|| format!("Failed to read image: {}", path.display()))?;

        let base64_content = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &content,
        );

        let mime_type = match path.extension().and_then(|e| e.to_str()) {
            Some("png") => "image/png",
            Some("jpg") | Some("jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            _ => "application/octet-stream",
        };

        Ok(ToolResult::success(
            "read",
            format!("data:{};base64,{}", mime_type, base64_content),
        ))
    }

    /// Write content to file
    async fn execute_write(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let file_path = args["file_path"]
            .as_str()
            .context("Missing 'file_path' parameter")?;

        let content = args["content"]
            .as_str()
            .context("Missing 'content' parameter")?;

        tracing::info!("Writing file: {} (workspace: {})", file_path, self.workspace.display());

        let resolved_path = self.resolve_path(file_path).or_else(|_| -> Result<PathBuf> {
            // If file doesn't exist yet, create path within workspace
            let path = Path::new(file_path);
            if path.is_absolute() {
                Ok(path.to_path_buf())
            } else {
                Ok(self.workspace.join(path))
            }
        })?;

        tracing::info!("Resolved absolute path: {}", resolved_path.display());

        // Create parent directories if needed
        if let Some(parent) = resolved_path.parent() {
            if !parent.exists() {
                tracing::info!("Creating parent directories: {}", parent.display());
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
            }
        }

        // Write file
        tokio::fs::write(&resolved_path, content)
            .await
            .with_context(|| format!("Failed to write file: {}", file_path))?;

        tracing::info!(
            "Successfully wrote {} bytes to {}",
            content.len(),
            resolved_path.display()
        );

        Ok(ToolResult::success(
            "write",
            format!(
                "Successfully wrote {} bytes to {}\nAbsolute path: {}",
                content.len(),
                file_path,
                resolved_path.display()
            ),
        ))
    }

    /// Edit file with string replacement
    async fn execute_edit(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let file_path = args["file_path"]
            .as_str()
            .context("Missing 'file_path' parameter")?;

        let old_string = args["old_string"]
            .as_str()
            .context("Missing 'old_string' parameter")?;

        let new_string = args["new_string"]
            .as_str()
            .context("Missing 'new_string' parameter")?;

        let replace_all = args["replace_all"].as_bool().unwrap_or(false);

        tracing::info!("Editing file: {} (workspace: {})", file_path, self.workspace.display());

        let resolved_path = self.resolve_path(file_path)?;

        tracing::debug!("Resolved absolute path: {}", resolved_path.display());

        // Read current content
        let content = tokio::fs::read_to_string(&resolved_path)
            .await
            .with_context(|| format!("Failed to read file: {}", file_path))?;

        // Check if old_string exists
        let count = content.matches(old_string).count();
        if count == 0 {
            return Ok(ToolResult::error(
                "edit",
                format!("String not found in {}: {:?}", file_path, old_string),
            ));
        }

        // Check for ambiguity
        if count > 1 && !replace_all {
            return Ok(ToolResult::error(
                "edit",
                format!(
                    "Found {} occurrences of the string in {}. Use replace_all=true to replace all, or provide more context to make it unique.",
                    count, file_path
                ),
            ));
        }

        // Perform replacement
        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        // Write back
        tokio::fs::write(&resolved_path, &new_content)
            .await
            .with_context(|| format!("Failed to write file: {}", file_path))?;

        // Generate diff for output
        let diff = generate_diff(&content, &new_content, file_path);

        Ok(ToolResult::success("edit", diff))
    }

    /// Search files with grep/ripgrep pattern
    async fn execute_grep(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let pattern = args["pattern"]
            .as_str()
            .context("Missing 'pattern' parameter")?;

        let path = args["path"].as_str().unwrap_or(".");
        let glob_filter = args["glob"].as_str();
        let context_lines = args["context"].as_u64().unwrap_or(0) as usize;
        let case_insensitive = args["-i"].as_bool().unwrap_or(false);

        let resolved_path = self.resolve_path(path)?;

        // Build ripgrep command
        let mut cmd = Command::new("rg");
        cmd.arg("--color=never")
            .arg("--line-number")
            .arg("--no-heading");

        if case_insensitive {
            cmd.arg("-i");
        }

        if context_lines > 0 {
            cmd.arg("-C").arg(context_lines.to_string());
        }

        if let Some(glob) = glob_filter {
            cmd.arg("--glob").arg(glob);
        }

        cmd.arg(pattern).arg(&resolved_path);

        let output = cmd.output().await.context("Failed to run ripgrep")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let mut result = stdout.to_string();
        if !stderr.is_empty() {
            result.push_str(&format!("\n{}", stderr));
        }

        // Truncate if too long
        if result.len() > MAX_OUTPUT_SIZE {
            result.truncate(MAX_OUTPUT_SIZE);
            result.push_str(&format!("\n\n[Output truncated at {} bytes]", MAX_OUTPUT_SIZE));
        }

        Ok(ToolResult {
            name: "grep".to_string(),
            output: result,
            exit_code: output.status.code().unwrap_or(0),
        })
    }

    /// Find files matching glob pattern
    async fn execute_glob(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let pattern = args["pattern"]
            .as_str()
            .context("Missing 'pattern' parameter")?;

        let path = args["path"].as_str();
        let base_path = if let Some(p) = path {
            self.resolve_path(p)?
        } else {
            self.workspace.clone()
        };

        // Use glob crate for pattern matching
        let full_pattern = base_path.join(pattern);
        let pattern_str = full_pattern.to_string_lossy();

        let mut files = Vec::new();
        for entry in glob::glob(&pattern_str).context("Invalid glob pattern")? {
            match entry {
                Ok(path) => {
                    // Get relative path from workspace
                    let relative = path
                        .strip_prefix(&self.workspace)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string();
                    files.push(relative);
                }
                Err(e) => {
                    tracing::warn!("Glob error: {}", e);
                }
            }
        }

        // Sort by modification time (newest first) if possible
        files.sort();

        let output = if files.is_empty() {
            format!("No files matching pattern: {}", pattern)
        } else {
            files.join("\n")
        };

        Ok(ToolResult::success("glob", output))
    }

    /// List directory contents
    async fn execute_ls(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let path = args["path"].as_str().unwrap_or(".");
        let resolved_path = self.resolve_path(path)?;

        if !resolved_path.is_dir() {
            return Ok(ToolResult::error(
                "ls",
                format!("{} is not a directory", path),
            ));
        }

        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(&resolved_path)
            .await
            .with_context(|| format!("Failed to read directory: {}", path))?;

        while let Some(entry) = dir.next_entry().await? {
            let metadata = entry.metadata().await?;
            let name = entry.file_name().to_string_lossy().to_string();

            let type_char = if metadata.is_dir() {
                "d"
            } else if metadata.is_symlink() {
                "l"
            } else {
                "-"
            };

            let size = metadata.len();
            entries.push(format!("{} {:>10} {}", type_char, size, name));
        }

        entries.sort();
        let output = entries.join("\n");

        Ok(ToolResult::success("ls", output))
    }
}

/// Check if file is an image based on extension
fn is_image_file(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => matches!(
            ext.to_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico"
        ),
        None => false,
    }
}

/// Generate unified diff between old and new content
fn generate_diff(old: &str, new: &str, file_path: &str) -> String {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(old, new);
    let mut output = String::new();

    output.push_str(&format!("--- a/{}\n", file_path));
    output.push_str(&format!("+++ b/{}\n", file_path));

    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        output.push_str(sign);
        output.push_str(change.value());
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_tool_executor_creation() {
        let executor = ToolExecutor::new("/tmp".to_string());
        assert_eq!(executor.workspace, PathBuf::from("/tmp"));
    }

    #[tokio::test]
    async fn test_unknown_tool() {
        let executor = ToolExecutor::new("/tmp".to_string());
        let result = executor
            .execute("unknown", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result.exit_code, 1);
        assert!(result.output.contains("Unknown tool"));
    }
}
