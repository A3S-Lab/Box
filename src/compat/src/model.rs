use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SourceLock {
    pub schema_version: u32,
    pub compatibility: CompatibilityLock,
    pub sources: BTreeMap<String, UpstreamSource>,
    pub files: Vec<LockedFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CompatibilityLock {
    pub id: String,
    pub version: String,
    pub control_plane_tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UpstreamSource {
    pub repository: String,
    pub commit: String,
    pub packages: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LockedFile {
    pub local_path: String,
    pub source: String,
    pub source_path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ContractInventory {
    pub schema_version: u32,
    pub compatibility_id: String,
    pub openapi: Vec<OpenApiInventory>,
    pub protobuf: Vec<ProtoFileInventory>,
    pub mcp: JsonSchemaInventory,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct OpenApiInventory {
    pub name: String,
    pub openapi_version: String,
    pub contract_version: String,
    pub operations: Vec<HttpOperation>,
    pub component_schemas: Vec<String>,
    pub fields: Vec<SchemaField>,
    pub authentication_headers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct HttpOperation {
    pub method: String,
    pub path: String,
    pub operation_id: Option<String>,
    pub tags: Vec<String>,
    pub parameters: Vec<HttpParameter>,
    pub request_content_types: Vec<String>,
    pub responses: Vec<HttpResponse>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct HttpParameter {
    pub name: Option<String>,
    pub location: Option<String>,
    pub required: bool,
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct HttpResponse {
    pub status: String,
    pub reference: Option<String>,
    pub content_types: Vec<String>,
    pub schema_references: Vec<String>,
    pub error: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SchemaField {
    pub pointer: String,
    pub name: String,
    pub required: bool,
    pub field_type: Option<String>,
    pub format: Option<String>,
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct JsonSchemaInventory {
    pub schema_id: Option<String>,
    pub title: Option<String>,
    pub fields: Vec<SchemaField>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ProtoFileInventory {
    pub path: String,
    pub package: String,
    pub descriptor_digest: String,
    pub services: Vec<ProtoService>,
    pub messages: Vec<ProtoMessage>,
    pub enums: Vec<ProtoEnum>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ProtoService {
    pub name: String,
    pub methods: Vec<ProtoMethod>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ProtoMethod {
    pub name: String,
    pub input_type: String,
    pub output_type: String,
    pub client_streaming: bool,
    pub server_streaming: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ProtoMessage {
    pub name: String,
    pub fields: Vec<ProtoField>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ProtoField {
    pub name: String,
    pub number: i32,
    pub label: String,
    pub field_type: String,
    pub type_name: Option<String>,
    pub oneof: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ProtoEnum {
    pub name: String,
    pub values: Vec<ProtoEnumValue>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ProtoEnumValue {
    pub name: String,
    pub number: i32,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct PublicExportInventory {
    pub schema_version: u32,
    pub compatibility_id: String,
    pub packages: BTreeMap<String, PackageExports>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct PackageExports {
    pub language: String,
    pub package: String,
    pub version: String,
    pub symbols: Vec<String>,
    pub type_only_symbols: Vec<String>,
    pub reexports: Vec<String>,
    pub has_default_export: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct CompatibilityManifest {
    pub schema_version: u32,
    pub compatibility_id: String,
    pub status: String,
    pub full_compatibility: bool,
    pub e2b_git_commit: String,
    pub code_interpreter_git_commit: String,
    pub python_e2b_version: String,
    pub typescript_e2b_version: String,
    pub python_code_interpreter_version: String,
    pub typescript_code_interpreter_version: String,
    pub control_openapi_digest: String,
    pub envd_openapi_digest: String,
    pub volume_content_openapi_digest: String,
    pub process_descriptor_digest: String,
    pub filesystem_descriptor_digest: String,
    pub mcp_schema_digest: String,
    pub contract_inventory_digest: String,
    pub public_export_inventory_digest: String,
    pub a3s_compat_version: String,
}
