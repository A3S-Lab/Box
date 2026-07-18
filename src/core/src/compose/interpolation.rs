//! Docker Compose-style variable interpolation for YAML scalar values.

use std::collections::HashMap;

use thiserror::Error;

const MAX_INTERPOLATION_DEPTH: usize = 32;

/// Failure while parsing or expanding Compose variable expressions.
#[derive(Debug, Error)]
pub enum ComposeInterpolationError {
    /// The Compose YAML could not be parsed or serialized.
    #[error("invalid Compose YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),

    /// A variable expression is malformed or uses an unsupported operator.
    #[error("invalid Compose variable expression at byte {offset}: {message}")]
    InvalidExpression {
        /// Byte offset of the opening `$` in its scalar value.
        offset: usize,
        /// Human-readable reason.
        message: String,
    },

    /// A required variable is unset or empty.
    #[error("required Compose variable '{name}' is unavailable: {message}")]
    RequiredVariable {
        /// Variable name.
        name: String,
        /// Expression-provided diagnostic.
        message: String,
    },
}

/// Expand Compose variables in YAML scalar values while leaving mapping keys intact.
pub fn interpolate_compose_yaml(
    input: &str,
    environment: &HashMap<String, String>,
) -> Result<String, ComposeInterpolationError> {
    let mut yaml: serde_yaml::Value = serde_yaml::from_str(input)?;
    interpolate_value(&mut yaml, environment)?;
    serde_yaml::to_string(&yaml).map_err(ComposeInterpolationError::from)
}

pub(super) fn interpolate_compose_scalar(
    input: &str,
    environment: &HashMap<String, String>,
) -> Result<String, ComposeInterpolationError> {
    interpolate_scalar(input, environment, 0)
}

fn interpolate_value(
    value: &mut serde_yaml::Value,
    environment: &HashMap<String, String>,
) -> Result<(), ComposeInterpolationError> {
    match value {
        serde_yaml::Value::String(scalar) => {
            *scalar = interpolate_scalar(scalar, environment, 0)?;
        }
        serde_yaml::Value::Sequence(sequence) => {
            for item in sequence {
                interpolate_value(item, environment)?;
            }
        }
        serde_yaml::Value::Mapping(mapping) => {
            // Compose interpolation applies to YAML values, not mapping keys.
            for item in mapping.values_mut() {
                interpolate_value(item, environment)?;
            }
        }
        serde_yaml::Value::Tagged(tagged) => {
            interpolate_value(&mut tagged.value, environment)?;
        }
        serde_yaml::Value::Null | serde_yaml::Value::Bool(_) | serde_yaml::Value::Number(_) => {}
    }
    Ok(())
}

fn interpolate_scalar(
    input: &str,
    environment: &HashMap<String, String>,
    depth: usize,
) -> Result<String, ComposeInterpolationError> {
    if depth > MAX_INTERPOLATION_DEPTH {
        return Err(invalid_expression(
            0,
            format!("nesting exceeds {MAX_INTERPOLATION_DEPTH} levels"),
        ));
    }

    let mut output = String::with_capacity(input.len());
    let mut offset = 0;

    while offset < input.len() {
        let rest = &input[offset..];
        let Some(character) = rest.chars().next() else {
            break;
        };

        if character != '$' {
            output.push(character);
            offset += character.len_utf8();
            continue;
        }

        if rest.starts_with("$$") {
            output.push('$');
            offset += 2;
            continue;
        }

        if rest.starts_with("${") {
            let closing = find_closing_brace(input, offset)?;
            let expression = &input[offset + 2..closing];
            output.push_str(&expand_expression(expression, environment, depth, offset)?);
            offset = closing + 1;
            continue;
        }

        let name_start = offset + 1;
        let name_end = scan_variable_name(input, name_start);
        if name_end == name_start {
            output.push('$');
            offset += 1;
            continue;
        }

        let name = &input[name_start..name_end];
        if let Some(value) = environment.get(name) {
            output.push_str(value);
        }
        offset = name_end;
    }

    Ok(output)
}

fn find_closing_brace(input: &str, opening: usize) -> Result<usize, ComposeInterpolationError> {
    let mut depth = 1usize;
    let mut offset = opening + 2;

    while offset < input.len() {
        let rest = &input[offset..];
        if rest.starts_with("${") {
            depth += 1;
            offset += 2;
            continue;
        }

        let Some(character) = rest.chars().next() else {
            break;
        };
        if character == '}' {
            depth -= 1;
            if depth == 0 {
                return Ok(offset);
            }
        }
        offset += character.len_utf8();
    }

    Err(invalid_expression(opening, "unterminated `${...}`"))
}

fn scan_variable_name(input: &str, start: usize) -> usize {
    let mut end = start;
    for (relative, character) in input[start..].char_indices() {
        let valid = if relative == 0 {
            character == '_' || character.is_ascii_alphabetic()
        } else {
            character == '_' || character.is_ascii_alphanumeric()
        };
        if !valid {
            break;
        }
        end = start + relative + character.len_utf8();
    }
    end
}

fn expand_expression(
    expression: &str,
    environment: &HashMap<String, String>,
    depth: usize,
    offset: usize,
) -> Result<String, ComposeInterpolationError> {
    let name_end = scan_variable_name(expression, 0);
    if name_end == 0 {
        return Err(invalid_expression(offset, "variable name is missing"));
    }

    let name = &expression[..name_end];
    let remainder = &expression[name_end..];
    let value = environment.get(name);
    let is_set = value.is_some();
    let is_nonempty = value.is_some_and(|value| !value.is_empty());

    if remainder.is_empty() {
        return Ok(value.cloned().unwrap_or_default());
    }

    let (operator, word) = [":-", ":+", ":?", "-", "+", "?"]
        .into_iter()
        .find_map(|operator| {
            remainder
                .strip_prefix(operator)
                .map(|word| (operator, word))
        })
        .ok_or_else(|| {
            invalid_expression(
                offset,
                format!("unsupported operator in `${{{expression}}}`"),
            )
        })?;

    let expand_word = || interpolate_scalar(word, environment, depth + 1);
    match operator {
        "-" if !is_set => expand_word(),
        ":-" if !is_nonempty => expand_word(),
        "+" if is_set => expand_word(),
        ":+" if is_nonempty => expand_word(),
        "?" if !is_set => required_variable(name, word, environment, depth),
        ":?" if !is_nonempty => required_variable(name, word, environment, depth),
        "-" | ":-" => Ok(value.cloned().unwrap_or_default()),
        "+" | ":+" => Ok(String::new()),
        "?" | ":?" => Ok(value.cloned().unwrap_or_default()),
        _ => unreachable!("operator matched the closed list above"),
    }
}

fn required_variable(
    name: &str,
    word: &str,
    environment: &HashMap<String, String>,
    depth: usize,
) -> Result<String, ComposeInterpolationError> {
    let message = if word.is_empty() {
        "variable is required".to_string()
    } else {
        interpolate_scalar(word, environment, depth + 1)?
    };
    Err(ComposeInterpolationError::RequiredVariable {
        name: name.to_string(),
        message,
    })
}

fn invalid_expression(offset: usize, message: impl Into<String>) -> ComposeInterpolationError {
    ComposeInterpolationError::InvalidExpression {
        offset,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn interpolated_value(yaml: &str, env: &[(&str, &str)]) -> serde_yaml::Value {
        let output = interpolate_compose_yaml(yaml, &environment(env)).unwrap();
        serde_yaml::from_str(&output).unwrap()
    }

    #[test]
    fn expands_unset_and_empty_default_operators() {
        let value = interpolated_value(
            r#"
dash_unset: ${UNSET-default}
dash_empty: ${EMPTY-default}
colon_dash_unset: ${UNSET:-default}
colon_dash_empty: ${EMPTY:-default}
"#,
            &[("EMPTY", "")],
        );

        assert_eq!(value["dash_unset"].as_str(), Some("default"));
        assert_eq!(value["dash_empty"].as_str(), Some(""));
        assert_eq!(value["colon_dash_unset"].as_str(), Some("default"));
        assert_eq!(value["colon_dash_empty"].as_str(), Some("default"));
    }

    #[test]
    fn expands_set_and_nonempty_replacement_operators() {
        let value = interpolated_value(
            r#"
plus_unset: ${UNSET+replacement}
plus_empty: ${EMPTY+replacement}
plus_value: ${VALUE+replacement}
colon_plus_unset: ${UNSET:+replacement}
colon_plus_empty: ${EMPTY:+replacement}
colon_plus_value: ${VALUE:+replacement}
"#,
            &[("EMPTY", ""), ("VALUE", "present")],
        );

        assert_eq!(value["plus_unset"].as_str(), Some(""));
        assert_eq!(value["plus_empty"].as_str(), Some("replacement"));
        assert_eq!(value["plus_value"].as_str(), Some("replacement"));
        assert_eq!(value["colon_plus_unset"].as_str(), Some(""));
        assert_eq!(value["colon_plus_empty"].as_str(), Some(""));
        assert_eq!(value["colon_plus_value"].as_str(), Some("replacement"));
    }

    #[test]
    fn expands_bare_braced_nested_and_escaped_dollars() {
        let value = interpolated_value(
            r#"
bare: $VALUE
braced: ${VALUE}
nested: ${UNSET:-${FALLBACK:-final}}
escaped: $$VALUE and $${VALUE}
"#,
            &[("VALUE", "resolved")],
        );

        assert_eq!(value["bare"].as_str(), Some("resolved"));
        assert_eq!(value["braced"].as_str(), Some("resolved"));
        assert_eq!(value["nested"].as_str(), Some("final"));
        assert_eq!(value["escaped"].as_str(), Some("$VALUE and ${VALUE}"));
    }

    #[test]
    fn interpolates_values_but_not_mapping_keys() {
        let value = interpolated_value(
            r#"
${KEY}: unchanged-key
environment:
  VALUE: ${VALUE:-fallback}
ports:
  - "${PORT:-6379}:6379"
"#,
            &[
                ("KEY", "expanded-key"),
                ("VALUE", "shell"),
                ("PORT", "16379"),
            ],
        );

        assert_eq!(value["${KEY}"].as_str(), Some("unchanged-key"));
        assert!(value.get("expanded-key").is_none());
        assert_eq!(value["environment"]["VALUE"].as_str(), Some("shell"));
        assert_eq!(value["ports"][0].as_str(), Some("16379:6379"));
    }

    #[test]
    fn reports_required_and_malformed_expressions() {
        let required =
            interpolate_compose_yaml("value: ${MISSING:?set MISSING}\n", &HashMap::new())
                .unwrap_err();
        assert!(required.to_string().contains("set MISSING"));

        let malformed =
            interpolate_compose_yaml("value: ${MISSING\n", &HashMap::new()).unwrap_err();
        assert!(malformed.to_string().contains("unterminated"));
    }
}
