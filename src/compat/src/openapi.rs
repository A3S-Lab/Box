use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::model::{
    HttpOperation, HttpParameter, HttpResponse, JsonSchemaInventory, OpenApiInventory, SchemaField,
};

const HTTP_METHODS: &[&str] = &[
    "delete", "get", "head", "options", "patch", "post", "put", "trace",
];

pub(crate) fn read_openapi(
    path: &Path,
    name: &str,
    allowed_tags: Option<&BTreeSet<String>>,
) -> Result<OpenApiInventory> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read OpenAPI contract {}", path.display()))?;
    let yaml: serde_yaml::Value = serde_yaml::from_slice(&bytes)
        .with_context(|| format!("failed to parse OpenAPI contract {}", path.display()))?;
    let document = serde_json::to_value(yaml)
        .with_context(|| format!("failed to normalize OpenAPI contract {}", path.display()))?;

    let openapi_version = string_at(&document, &["openapi"]).unwrap_or_default();
    let contract_version = string_at(&document, &["info", "version"]).unwrap_or_default();
    let mut operations = collect_operations(&document, allowed_tags);
    operations.sort_by(|left, right| {
        (&left.path, &left.method, &left.operation_id).cmp(&(
            &right.path,
            &right.method,
            &right.operation_id,
        ))
    });

    let mut fields = Vec::new();
    collect_schema_fields(&document, "", &mut fields);
    fields.sort();
    fields.dedup();

    let mut authentication_headers = collect_authentication_headers(&document, &operations);
    authentication_headers.sort();
    authentication_headers.dedup();

    let mut component_schemas = document
        .pointer("/components/schemas")
        .and_then(Value::as_object)
        .map(|schemas| schemas.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    component_schemas.sort();

    Ok(OpenApiInventory {
        name: name.to_string(),
        openapi_version,
        contract_version,
        operations,
        component_schemas,
        fields,
        authentication_headers,
    })
}

pub(crate) fn read_json_schema(path: &Path) -> Result<JsonSchemaInventory> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read JSON schema {}", path.display()))?;
    let document: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse JSON schema {}", path.display()))?;
    let mut fields = Vec::new();
    collect_schema_fields(&document, "", &mut fields);
    fields.sort();
    fields.dedup();
    Ok(JsonSchemaInventory {
        schema_id: value_string(document.get("$id").or_else(|| document.get("id"))),
        title: value_string(document.get("title")),
        fields,
    })
}

fn collect_operations(
    document: &Value,
    allowed_tags: Option<&BTreeSet<String>>,
) -> Vec<HttpOperation> {
    let Some(paths) = document.get("paths").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut operations = Vec::new();
    for (path, path_item) in paths {
        let Some(path_object) = path_item.as_object() else {
            continue;
        };
        let path_parameters = path_object.get("parameters").and_then(Value::as_array);
        for method in HTTP_METHODS {
            let Some(operation) = path_object.get(*method).and_then(Value::as_object) else {
                continue;
            };
            let mut parameters = Vec::new();
            if let Some(items) = path_parameters {
                parameters.extend(items.iter().map(parameter_inventory));
            }
            if let Some(items) = operation.get("parameters").and_then(Value::as_array) {
                parameters.extend(items.iter().map(parameter_inventory));
            }
            parameters.sort_by(|left, right| {
                (&left.location, &left.name, &left.reference).cmp(&(
                    &right.location,
                    &right.name,
                    &right.reference,
                ))
            });

            let request_content_types = operation
                .get("requestBody")
                .and_then(|body| body.get("content"))
                .and_then(Value::as_object)
                .map(|content| content.keys().cloned().collect())
                .unwrap_or_default();
            let mut responses = operation
                .get("responses")
                .and_then(Value::as_object)
                .map(|items| {
                    items
                        .iter()
                        .map(|(status, response)| response_inventory(status, response))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            responses.sort_by(|left, right| left.status.cmp(&right.status));

            let tags = operation
                .get("tags")
                .and_then(Value::as_array)
                .map(|tags| {
                    tags.iter()
                        .filter_map(|tag| value_string(Some(tag)))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if allowed_tags.is_some_and(|allowed| !tags.iter().any(|tag| allowed.contains(tag))) {
                continue;
            }

            operations.push(HttpOperation {
                method: method.to_ascii_uppercase(),
                path: path.clone(),
                operation_id: value_string(operation.get("operationId")),
                tags,
                parameters,
                request_content_types,
                responses,
            });
        }
    }
    operations
}

fn parameter_inventory(parameter: &Value) -> HttpParameter {
    HttpParameter {
        name: value_string(parameter.get("name")),
        location: value_string(parameter.get("in")),
        required: parameter
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        reference: value_string(parameter.get("$ref")),
    }
}

fn response_inventory(status: &str, response: &Value) -> HttpResponse {
    let mut schema_references = BTreeSet::new();
    collect_references(response, &mut schema_references);
    let content_types = response
        .get("content")
        .and_then(Value::as_object)
        .map(|content| content.keys().cloned().collect())
        .unwrap_or_default();
    HttpResponse {
        status: status.to_string(),
        reference: value_string(response.get("$ref")),
        content_types,
        schema_references: schema_references.into_iter().collect(),
        error: !status.starts_with('2'),
    }
}

fn collect_references(value: &Value, references: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                references.insert(reference.to_string());
            }
            for child in object.values() {
                collect_references(child, references);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_references(child, references);
            }
        }
        _ => {}
    }
}

fn collect_schema_fields(value: &Value, pointer: &str, fields: &mut Vec<SchemaField>) {
    match value {
        Value::Object(object) => {
            if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                let required = object
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<BTreeSet<_>>()
                    })
                    .unwrap_or_default();
                for (name, property) in properties {
                    let property_pointer =
                        format!("{pointer}/properties/{}", escape_json_pointer_segment(name));
                    fields.push(SchemaField {
                        pointer: property_pointer,
                        name: name.clone(),
                        required: required.contains(name.as_str()),
                        field_type: schema_type(property),
                        format: value_string(property.get("format")),
                        reference: first_reference(property),
                    });
                }
            }
            for (key, child) in object {
                let child_pointer = format!("{pointer}/{}", escape_json_pointer_segment(key));
                collect_schema_fields(child, &child_pointer, fields);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                collect_schema_fields(child, &format!("{pointer}/{index}"), fields);
            }
        }
        _ => {}
    }
}

fn collect_authentication_headers(document: &Value, operations: &[HttpOperation]) -> Vec<String> {
    let mut headers = BTreeSet::new();
    if let Some(schemes) = document
        .pointer("/components/securitySchemes")
        .and_then(Value::as_object)
    {
        for scheme in schemes.values() {
            let scheme_type = scheme.get("type").and_then(Value::as_str);
            let scheme_location = scheme.get("in").and_then(Value::as_str);
            let scheme_name = scheme.get("scheme").and_then(Value::as_str);
            if scheme_type == Some("apiKey")
                && (scheme_location == Some("header") || scheme_name == Some("header"))
            {
                if let Some(name) = scheme.get("name").and_then(Value::as_str) {
                    headers.insert(name.to_string());
                }
            } else if scheme_type == Some("http") && scheme_name == Some("bearer") {
                headers.insert("Authorization".to_string());
            }
        }
    }
    if let Some(parameters) = document
        .pointer("/components/parameters")
        .and_then(Value::as_object)
    {
        for parameter in parameters.values() {
            if parameter.get("in").and_then(Value::as_str) == Some("header") {
                if let Some(name) = parameter.get("name").and_then(Value::as_str) {
                    headers.insert(name.to_string());
                }
            }
        }
    }
    for operation in operations {
        for parameter in &operation.parameters {
            if parameter.location.as_deref() == Some("header") {
                if let Some(name) = parameter.name.as_ref() {
                    headers.insert(name.clone());
                }
            }
        }
    }
    headers.into_iter().collect()
}

fn first_reference(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                return Some(reference.to_string());
            }
            object.values().find_map(first_reference)
        }
        Value::Array(items) => items.iter().find_map(first_reference),
        _ => None,
    }
}

fn schema_type(value: &Value) -> Option<String> {
    match value.get("type") {
        Some(Value::String(value)) => Some(value.clone()),
        Some(Value::Array(values)) => Some(
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("|"),
        ),
        _ => None,
    }
}

fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    value_string(Some(current))
}

fn value_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(value)) => Some(value.clone()),
        Some(Value::Number(value)) => Some(value.to_string()),
        _ => None,
    }
}

fn escape_json_pointer_segment(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_inline_fields_and_error_responses() {
        let document: Value = serde_json::json!({
            "openapi": "3.0.0",
            "info": {"version": "1"},
            "paths": {
                "/sandboxes": {
                    "post": {
                        "operationId": "createSandbox",
                        "requestBody": {"content": {"application/json": {"schema": {
                            "type": "object",
                            "required": ["template"],
                            "properties": {"template": {"type": "string"}}
                        }}}},
                        "responses": {
                            "201": {"description": "created"},
                            "400": {"$ref": "#/components/responses/BadRequest"}
                        }
                    }
                }
            }
        });
        let operations = collect_operations(&document, None);
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].request_content_types, ["application/json"]);
        assert!(!operations[0].responses[0].error);
        assert!(operations[0].responses[1].error);

        let mut fields = Vec::new();
        collect_schema_fields(&document, "", &mut fields);
        assert!(fields
            .iter()
            .any(|field| field.name == "template" && field.required));
    }
}
