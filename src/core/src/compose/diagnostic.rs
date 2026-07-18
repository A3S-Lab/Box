//! Structured diagnostics produced while parsing and normalizing Compose input.

use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Stable machine-readable category for a Compose diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ComposeDiagnosticCode {
    /// The source is not syntactically valid YAML or ACL.
    #[serde(rename = "compose.syntax")]
    Syntax,
    /// Environment interpolation could not be completed.
    #[serde(rename = "compose.interpolation")]
    Interpolation,
    /// The source contains a field outside Box's closed Compose schema.
    #[serde(rename = "compose.unsupported_field")]
    UnsupportedField,
    /// A recognized field contains a value that Box cannot implement.
    #[serde(rename = "compose.unsupported_value")]
    UnsupportedValue,
    /// A recognized field contains an invalid value.
    #[serde(rename = "compose.invalid_value")]
    InvalidValue,
}

impl ComposeDiagnosticCode {
    /// Stable dotted representation suitable for logs and API responses.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Syntax => "compose.syntax",
            Self::Interpolation => "compose.interpolation",
            Self::UnsupportedField => "compose.unsupported_field",
            Self::UnsupportedValue => "compose.unsupported_value",
            Self::InvalidValue => "compose.invalid_value",
        }
    }
}

impl fmt::Display for ComposeDiagnosticCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One actionable Compose parsing or normalization failure.
///
/// `path` uses JSON Pointer-style segments so callers do not need to parse a
/// human error string. ACL blocks are projected onto the equivalent Compose
/// object path (for example, `service "api"` becomes `/services/api`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposeDiagnostic {
    /// Stable machine-readable failure category.
    pub code: ComposeDiagnosticCode,
    /// JSON Pointer-style path to the rejected field or value.
    pub path: String,
    /// Human-readable explanation.
    pub message: String,
    /// One-based source line when the parser exposes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// One-based source column when the parser exposes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
}

impl ComposeDiagnostic {
    pub(super) fn new(
        code: ComposeDiagnosticCode,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            path: path.into(),
            message: message.into(),
            line: None,
            column: None,
        }
    }

    pub(super) fn with_location(mut self, line: usize, column: usize) -> Self {
        self.line = Some(line);
        self.column = Some(column);
        self
    }

    pub(super) fn unsupported_field(path: impl Into<String>, field: &str) -> Self {
        Self::new(
            ComposeDiagnosticCode::UnsupportedField,
            path,
            format!("unsupported Compose field {field:?}"),
        )
    }
}

impl fmt::Display for ComposeDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at {}: {}",
            self.code, self.path, self.message
        )?;
        if let (Some(line), Some(column)) = (self.line, self.column) {
            write!(formatter, " (line {line}, column {column})")?;
        }
        Ok(())
    }
}

/// One or more structured failures from a pure Compose normalization pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeNormalizationError {
    diagnostics: Vec<ComposeDiagnostic>,
}

impl ComposeNormalizationError {
    pub(super) fn new(mut diagnostics: Vec<ComposeDiagnostic>) -> Self {
        diagnostics.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.code.cmp(&right.code))
                .then_with(|| left.message.cmp(&right.message))
        });
        diagnostics.dedup();
        Self { diagnostics }
    }

    pub(super) fn one(diagnostic: ComposeDiagnostic) -> Self {
        Self::new(vec![diagnostic])
    }

    /// Structured diagnostics in deterministic path/code/message order.
    pub fn diagnostics(&self) -> &[ComposeDiagnostic] {
        &self.diagnostics
    }

    /// Consume the error and return its structured diagnostics.
    pub fn into_diagnostics(self) -> Vec<ComposeDiagnostic> {
        self.diagnostics
    }
}

impl fmt::Display for ComposeNormalizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.diagnostics.as_slice() {
            [] => formatter.write_str("Compose normalization failed"),
            [diagnostic] => diagnostic.fmt(formatter),
            diagnostics => {
                write!(
                    formatter,
                    "Compose normalization failed with {} diagnostics: ",
                    diagnostics.len()
                )?;
                for (index, diagnostic) in diagnostics.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str("; ")?;
                    }
                    diagnostic.fmt(formatter)?;
                }
                Ok(())
            }
        }
    }
}

impl Error for ComposeNormalizationError {}

pub(super) fn pointer_segment(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

pub(super) fn child_path(parent: &str, child: &str) -> String {
    let child = pointer_segment(child);
    if parent == "/" {
        format!("/{child}")
    } else {
        format!("{parent}/{child}")
    }
}
