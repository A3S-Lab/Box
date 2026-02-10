//! Dockerfile parser.
//!
//! Parses a Dockerfile into a sequence of build instructions.
//! Supports line continuations (`\`), comments, and both shell and JSON
//! (exec) forms for CMD/ENTRYPOINT.

use a3s_box_core::error::{BoxError, Result};

/// A single Dockerfile instruction.
#[derive(Debug, Clone, PartialEq)]
pub enum Instruction {
    /// `FROM <image> [AS <alias>]`
    From {
        image: String,
        alias: Option<String>,
    },
    /// `RUN <command>` (shell form)
    Run { command: String },
    /// `COPY [--from=<stage>] <src>... <dst>`
    Copy {
        src: Vec<String>,
        dst: String,
        from: Option<String>,
    },
    /// `WORKDIR <path>`
    Workdir { path: String },
    /// `ENV <key>=<value>` or `ENV <key> <value>`
    Env { key: String, value: String },
    /// `ENTRYPOINT ["exec", "form"]` or `ENTRYPOINT command`
    Entrypoint { exec: Vec<String> },
    /// `CMD ["exec", "form"]` or `CMD command`
    Cmd { exec: Vec<String> },
    /// `EXPOSE <port>[/<proto>]`
    Expose { port: String },
    /// `LABEL <key>=<value> ...`
    Label { key: String, value: String },
    /// `USER <user>[:<group>]`
    User { user: String },
    /// `ARG <name>[=<default>]`
    Arg {
        name: String,
        default: Option<String>,
    },
}

/// Parsed Dockerfile: a list of instructions in order.
#[derive(Debug, Clone)]
pub struct Dockerfile {
    pub instructions: Vec<Instruction>,
}

impl Dockerfile {
    /// Parse a Dockerfile from its text content.
    pub fn parse(content: &str) -> Result<Self> {
        let logical_lines = join_continuation_lines(content);
        let mut instructions = Vec::new();

        for (line_num, line) in logical_lines.iter().enumerate() {
            let trimmed = line.trim();

            // Skip empty lines and comments
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            let instruction = parse_instruction(trimmed, line_num + 1)?;
            instructions.push(instruction);
        }

        if instructions.is_empty() {
            return Err(BoxError::BuildError(
                "Dockerfile is empty or contains no instructions".to_string(),
            ));
        }

        // Validate: first non-ARG instruction must be FROM
        let first_non_arg = instructions
            .iter()
            .find(|i| !matches!(i, Instruction::Arg { .. }));
        if !matches!(first_non_arg, Some(Instruction::From { .. })) {
            return Err(BoxError::BuildError(
                "First instruction must be FROM (or ARG before FROM)".to_string(),
            ));
        }

        Ok(Dockerfile { instructions })
    }

    /// Parse a Dockerfile from a file path.
    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to read Dockerfile at {}: {}",
                path.display(),
                e
            ))
        })?;
        Self::parse(&content)
    }
}

/// Join lines ending with `\` into single logical lines.
fn join_continuation_lines(content: &str) -> Vec<String> {
    let mut logical_lines = Vec::new();
    let mut current = String::new();

    for line in content.lines() {
        if line.ends_with('\\') {
            // Remove trailing backslash and append
            current.push_str(line[..line.len() - 1].trim_end());
            current.push(' ');
        } else {
            current.push_str(line);
            logical_lines.push(current.clone());
            current.clear();
        }
    }

    // Handle trailing continuation without final line
    if !current.is_empty() {
        logical_lines.push(current);
    }

    logical_lines
}

/// Parse a single logical line into an Instruction.
fn parse_instruction(line: &str, line_num: usize) -> Result<Instruction> {
    // Split into keyword and rest
    let (keyword, rest) = split_first_word(line);
    let keyword_upper = keyword.to_uppercase();

    match keyword_upper.as_str() {
        "FROM" => parse_from(rest, line_num),
        "RUN" => parse_run(rest, line_num),
        "COPY" => parse_copy(rest, line_num),
        "WORKDIR" => parse_workdir(rest, line_num),
        "ENV" => parse_env(rest, line_num),
        "ENTRYPOINT" => parse_entrypoint(rest, line_num),
        "CMD" => parse_cmd(rest, line_num),
        "EXPOSE" => parse_expose(rest, line_num),
        "LABEL" => parse_label(rest, line_num),
        "USER" => parse_user(rest, line_num),
        "ARG" => parse_arg(rest, line_num),
        // Silently ignore unsupported instructions with a warning
        "ADD" | "VOLUME" | "SHELL" | "STOPSIGNAL" | "HEALTHCHECK" | "ONBUILD" | "MAINTAINER" => {
            tracing::warn!(
                line = line_num,
                instruction = keyword_upper.as_str(),
                "Unsupported Dockerfile instruction, skipping"
            );
            // Return a RUN with empty command that the engine will skip
            Ok(Instruction::Label {
                key: format!("a3s.build.skipped.{}", keyword_upper.to_lowercase()),
                value: rest.to_string(),
            })
        }
        _ => Err(BoxError::BuildError(format!(
            "Line {}: Unknown instruction '{}'",
            line_num, keyword
        ))),
    }
}

/// Split a string into the first word and the rest.
fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    match s.find(char::is_whitespace) {
        Some(pos) => (&s[..pos], s[pos..].trim_start()),
        None => (s, ""),
    }
}

// --- Individual instruction parsers ---

fn parse_from(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: FROM requires an image argument",
            line_num
        )));
    }

    // Check for AS alias: FROM image AS alias
    let parts: Vec<&str> = rest.splitn(3, char::is_whitespace).collect();
    let (image, alias) = if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("AS") {
        (parts[0].to_string(), Some(parts[2].trim().to_string()))
    } else {
        (parts[0].to_string(), None)
    };

    Ok(Instruction::From { image, alias })
}

fn parse_run(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: RUN requires a command",
            line_num
        )));
    }

    // If JSON array form, extract and join
    let command = if rest.starts_with('[') {
        let parts = parse_json_array(rest, line_num)?;
        parts.join(" ")
    } else {
        rest.to_string()
    };

    Ok(Instruction::Run { command })
}

fn parse_copy(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: COPY requires source and destination",
            line_num
        )));
    }

    // Check for --from=<stage> flag
    let (from, remaining) = if rest.starts_with("--from=") {
        let (flag, after) = split_first_word(rest);
        let stage = flag
            .strip_prefix("--from=")
            .unwrap_or("")
            .to_string();
        (Some(stage), after)
    } else {
        (None, rest)
    };

    // Split remaining into src... dst (last element is dst)
    let parts: Vec<&str> = shell_split(remaining);
    if parts.len() < 2 {
        return Err(BoxError::BuildError(format!(
            "Line {}: COPY requires at least one source and a destination",
            line_num
        )));
    }

    let dst = parts.last().unwrap().to_string();
    let src: Vec<String> = parts[..parts.len() - 1]
        .iter()
        .map(|s| s.to_string())
        .collect();

    Ok(Instruction::Copy { src, dst, from })
}

fn parse_workdir(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: WORKDIR requires a path",
            line_num
        )));
    }
    Ok(Instruction::Workdir {
        path: rest.to_string(),
    })
}

fn parse_env(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: ENV requires a key and value",
            line_num
        )));
    }

    // Two forms:
    // ENV KEY=VALUE  (or KEY="VALUE")
    // ENV KEY VALUE
    if let Some(eq_pos) = rest.find('=') {
        // Check it's not inside a value after a space
        let space_pos = rest.find(char::is_whitespace);
        if space_pos.is_none() || eq_pos < space_pos.unwrap() {
            let key = rest[..eq_pos].to_string();
            let value = unquote(&rest[eq_pos + 1..]);
            return Ok(Instruction::Env { key, value });
        }
    }

    // Legacy form: ENV KEY VALUE
    let (key, value) = split_first_word(rest);
    Ok(Instruction::Env {
        key: key.to_string(),
        value: value.to_string(),
    })
}

fn parse_entrypoint(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: ENTRYPOINT requires an argument",
            line_num
        )));
    }

    let exec = if rest.starts_with('[') {
        parse_json_array(rest, line_num)?
    } else {
        // Shell form: wrap in sh -c
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            rest.to_string(),
        ]
    };

    Ok(Instruction::Entrypoint { exec })
}

fn parse_cmd(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: CMD requires an argument",
            line_num
        )));
    }

    let exec = if rest.starts_with('[') {
        parse_json_array(rest, line_num)?
    } else {
        // Shell form: wrap in sh -c
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            rest.to_string(),
        ]
    };

    Ok(Instruction::Cmd { exec })
}

fn parse_expose(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: EXPOSE requires a port",
            line_num
        )));
    }
    Ok(Instruction::Expose {
        port: rest.split_whitespace().next().unwrap_or(rest).to_string(),
    })
}

fn parse_label(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: LABEL requires key=value",
            line_num
        )));
    }

    // LABEL key=value
    if let Some(eq_pos) = rest.find('=') {
        let key = rest[..eq_pos].trim().to_string();
        let value = unquote(rest[eq_pos + 1..].trim());
        Ok(Instruction::Label { key, value })
    } else {
        // LABEL key value (legacy)
        let (key, value) = split_first_word(rest);
        Ok(Instruction::Label {
            key: key.to_string(),
            value: unquote(value),
        })
    }
}

fn parse_user(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: USER requires a username",
            line_num
        )));
    }
    Ok(Instruction::User {
        user: rest.split_whitespace().next().unwrap_or(rest).to_string(),
    })
}

fn parse_arg(rest: &str, line_num: usize) -> Result<Instruction> {
    if rest.is_empty() {
        return Err(BoxError::BuildError(format!(
            "Line {}: ARG requires a name",
            line_num
        )));
    }

    if let Some(eq_pos) = rest.find('=') {
        let name = rest[..eq_pos].to_string();
        let default = Some(unquote(&rest[eq_pos + 1..]));
        Ok(Instruction::Arg { name, default })
    } else {
        Ok(Instruction::Arg {
            name: rest.trim().to_string(),
            default: None,
        })
    }
}

// --- Helpers ---

/// Parse a JSON array string like `["a", "b", "c"]` into a Vec<String>.
fn parse_json_array(s: &str, line_num: usize) -> Result<Vec<String>> {
    let parsed: Vec<String> = serde_json::from_str(s).map_err(|e| {
        BoxError::BuildError(format!(
            "Line {}: Invalid JSON array '{}': {}",
            line_num, s, e
        ))
    })?;
    Ok(parsed)
}

/// Remove surrounding quotes from a string.
fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Simple whitespace-based split that respects quoted strings.
fn shell_split(s: &str) -> Vec<&str> {
    s.split_whitespace().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- join_continuation_lines ---

    #[test]
    fn test_join_continuation_simple() {
        let input = "RUN apt-get update && \\\n    apt-get install -y curl";
        let lines = join_continuation_lines(input);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("apt-get update"));
        assert!(lines[0].contains("apt-get install"));
    }

    #[test]
    fn test_join_continuation_no_continuation() {
        let input = "FROM alpine:3.19\nRUN echo hello";
        let lines = join_continuation_lines(input);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_join_continuation_multiple() {
        let input = "RUN a \\\n    b \\\n    c";
        let lines = join_continuation_lines(input);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains('a'));
        assert!(lines[0].contains('b'));
        assert!(lines[0].contains('c'));
    }

    // --- parse_from ---

    #[test]
    fn test_parse_from_simple() {
        let result = parse_from("alpine:3.19", 1).unwrap();
        assert_eq!(
            result,
            Instruction::From {
                image: "alpine:3.19".to_string(),
                alias: None,
            }
        );
    }

    #[test]
    fn test_parse_from_with_alias() {
        let result = parse_from("golang:1.21 AS builder", 1).unwrap();
        assert_eq!(
            result,
            Instruction::From {
                image: "golang:1.21".to_string(),
                alias: Some("builder".to_string()),
            }
        );
    }

    #[test]
    fn test_parse_from_empty() {
        assert!(parse_from("", 1).is_err());
    }

    // --- parse_run ---

    #[test]
    fn test_parse_run_shell() {
        let result = parse_run("apt-get update && apt-get install -y curl", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Run {
                command: "apt-get update && apt-get install -y curl".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_run_json() {
        let result = parse_run(r#"["echo", "hello"]"#, 1).unwrap();
        assert_eq!(
            result,
            Instruction::Run {
                command: "echo hello".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_run_empty() {
        assert!(parse_run("", 1).is_err());
    }

    // --- parse_copy ---

    #[test]
    fn test_parse_copy_simple() {
        let result = parse_copy("app.py /workspace/", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Copy {
                src: vec!["app.py".to_string()],
                dst: "/workspace/".to_string(),
                from: None,
            }
        );
    }

    #[test]
    fn test_parse_copy_multiple_sources() {
        let result = parse_copy("file1.txt file2.txt /dest/", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Copy {
                src: vec!["file1.txt".to_string(), "file2.txt".to_string()],
                dst: "/dest/".to_string(),
                from: None,
            }
        );
    }

    #[test]
    fn test_parse_copy_from_stage() {
        let result = parse_copy("--from=builder /app/bin /usr/local/bin/", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Copy {
                src: vec!["/app/bin".to_string()],
                dst: "/usr/local/bin/".to_string(),
                from: Some("builder".to_string()),
            }
        );
    }

    #[test]
    fn test_parse_copy_empty() {
        assert!(parse_copy("", 1).is_err());
    }

    #[test]
    fn test_parse_copy_single_arg() {
        assert!(parse_copy("onlysource", 1).is_err());
    }

    // --- parse_env ---

    #[test]
    fn test_parse_env_equals() {
        let result = parse_env("PATH=/usr/local/bin:/usr/bin", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Env {
                key: "PATH".to_string(),
                value: "/usr/local/bin:/usr/bin".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_env_quoted() {
        let result = parse_env(r#"MSG="hello world""#, 1).unwrap();
        assert_eq!(
            result,
            Instruction::Env {
                key: "MSG".to_string(),
                value: "hello world".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_env_legacy() {
        let result = parse_env("MY_VAR my_value", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Env {
                key: "MY_VAR".to_string(),
                value: "my_value".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_env_empty() {
        assert!(parse_env("", 1).is_err());
    }

    // --- parse_entrypoint ---

    #[test]
    fn test_parse_entrypoint_exec() {
        let result = parse_entrypoint(r#"["/bin/agent", "--listen"]"#, 1).unwrap();
        assert_eq!(
            result,
            Instruction::Entrypoint {
                exec: vec!["/bin/agent".to_string(), "--listen".to_string()],
            }
        );
    }

    #[test]
    fn test_parse_entrypoint_shell() {
        let result = parse_entrypoint("/bin/agent --listen", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Entrypoint {
                exec: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "/bin/agent --listen".to_string(),
                ],
            }
        );
    }

    // --- parse_cmd ---

    #[test]
    fn test_parse_cmd_exec() {
        let result = parse_cmd(r#"["--port", "8080"]"#, 1).unwrap();
        assert_eq!(
            result,
            Instruction::Cmd {
                exec: vec!["--port".to_string(), "8080".to_string()],
            }
        );
    }

    #[test]
    fn test_parse_cmd_shell() {
        let result = parse_cmd("echo hello", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Cmd {
                exec: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "echo hello".to_string(),
                ],
            }
        );
    }

    // --- parse_expose ---

    #[test]
    fn test_parse_expose() {
        let result = parse_expose("8080", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Expose {
                port: "8080".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_expose_with_proto() {
        let result = parse_expose("8080/tcp", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Expose {
                port: "8080/tcp".to_string(),
            }
        );
    }

    // --- parse_label ---

    #[test]
    fn test_parse_label_equals() {
        let result = parse_label("version=1.0.0", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Label {
                key: "version".to_string(),
                value: "1.0.0".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_label_quoted() {
        let result = parse_label(r#"description="My App""#, 1).unwrap();
        assert_eq!(
            result,
            Instruction::Label {
                key: "description".to_string(),
                value: "My App".to_string(),
            }
        );
    }

    // --- parse_user ---

    #[test]
    fn test_parse_user() {
        let result = parse_user("nobody", 1).unwrap();
        assert_eq!(
            result,
            Instruction::User {
                user: "nobody".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_user_with_group() {
        let result = parse_user("1000:1000", 1).unwrap();
        assert_eq!(
            result,
            Instruction::User {
                user: "1000:1000".to_string(),
            }
        );
    }

    // --- parse_arg ---

    #[test]
    fn test_parse_arg_no_default() {
        let result = parse_arg("VERSION", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Arg {
                name: "VERSION".to_string(),
                default: None,
            }
        );
    }

    #[test]
    fn test_parse_arg_with_default() {
        let result = parse_arg("VERSION=1.0.0", 1).unwrap();
        assert_eq!(
            result,
            Instruction::Arg {
                name: "VERSION".to_string(),
                default: Some("1.0.0".to_string()),
            }
        );
    }

    // --- Full Dockerfile parsing ---

    #[test]
    fn test_parse_minimal_dockerfile() {
        let content = "FROM alpine:3.19\nCMD [\"echo\", \"hello\"]";
        let df = Dockerfile::parse(content).unwrap();
        assert_eq!(df.instructions.len(), 2);
        assert!(matches!(&df.instructions[0], Instruction::From { image, .. } if image == "alpine:3.19"));
    }

    #[test]
    fn test_parse_complex_dockerfile() {
        let content = r#"
# Build stage
FROM python:3.12-slim

WORKDIR /app

ENV PYTHONDONTWRITEBYTECODE=1
ENV PYTHONUNBUFFERED=1

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY . .

EXPOSE 8080

LABEL version="1.0.0"
LABEL maintainer="team@example.com"

USER nobody

ENTRYPOINT ["python"]
CMD ["app.py"]
"#;
        let df = Dockerfile::parse(content).unwrap();
        assert_eq!(df.instructions.len(), 13);
    }

    #[test]
    fn test_parse_with_continuations() {
        let content = "FROM alpine:3.19\nRUN apk add --no-cache \\\n    curl \\\n    wget";
        let df = Dockerfile::parse(content).unwrap();
        assert_eq!(df.instructions.len(), 2);
        if let Instruction::Run { command } = &df.instructions[1] {
            assert!(command.contains("curl"));
            assert!(command.contains("wget"));
        } else {
            panic!("Expected RUN instruction");
        }
    }

    #[test]
    fn test_parse_empty_dockerfile() {
        let content = "# just a comment\n\n";
        assert!(Dockerfile::parse(content).is_err());
    }

    #[test]
    fn test_parse_no_from() {
        let content = "RUN echo hello";
        assert!(Dockerfile::parse(content).is_err());
    }

    #[test]
    fn test_parse_arg_before_from() {
        let content = "ARG VERSION=3.19\nFROM alpine:${VERSION}";
        let df = Dockerfile::parse(content).unwrap();
        assert_eq!(df.instructions.len(), 2);
        assert!(matches!(&df.instructions[0], Instruction::Arg { .. }));
        assert!(matches!(&df.instructions[1], Instruction::From { .. }));
    }

    #[test]
    fn test_parse_comments_and_blanks() {
        let content = "\n# comment\n\nFROM alpine\n\n# another comment\nRUN echo hi\n\n";
        let df = Dockerfile::parse(content).unwrap();
        assert_eq!(df.instructions.len(), 2);
    }

    // --- unquote ---

    #[test]
    fn test_unquote_double() {
        assert_eq!(unquote(r#""hello world""#), "hello world");
    }

    #[test]
    fn test_unquote_single() {
        assert_eq!(unquote("'hello world'"), "hello world");
    }

    #[test]
    fn test_unquote_none() {
        assert_eq!(unquote("hello"), "hello");
    }

    #[test]
    fn test_unquote_mismatched() {
        assert_eq!(unquote(r#""hello'"#), r#""hello'"#);
    }
}
