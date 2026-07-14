use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use prost::Message;
use prost_types::{
    field_descriptor_proto::{Label, Type},
    DescriptorProto, EnumDescriptorProto, FileDescriptorProto, FileDescriptorSet,
};

use crate::digest::sha256;
use crate::model::{
    ProtoEnum, ProtoEnumValue, ProtoField, ProtoFileInventory, ProtoMessage, ProtoMethod,
    ProtoService,
};
use serde::Serialize;

pub(crate) fn read_protobuf_contracts(
    proto_root: &Path,
    relative_paths: &[&str],
) -> Result<Vec<ProtoFileInventory>> {
    let descriptor = tempfile::NamedTempFile::new()
        .context("failed to create temporary Protobuf descriptor file")?;
    let descriptor_path = descriptor.path().to_path_buf();
    let mut command = Command::new("protoc");
    command
        .current_dir(proto_root)
        .arg("--include_imports")
        .arg("--proto_path=.")
        .arg(format!(
            "--descriptor_set_out={}",
            descriptor_path.display()
        ));
    for include in [
        "/usr/include",
        "/usr/local/include",
        "/opt/homebrew/include",
        "/usr/local/opt/protobuf/include",
    ] {
        if Path::new(include).is_dir() {
            command.arg(format!("--proto_path={include}"));
        }
    }
    for path in relative_paths {
        command.arg(path);
    }
    let output = command.output().with_context(|| {
        format!(
            "failed to execute protoc for contracts below {}",
            proto_root.display()
        )
    })?;
    if !output.status.success() {
        bail!(
            "protoc failed for contracts below {}: {}",
            proto_root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let bytes = std::fs::read(&descriptor_path)
        .context("failed to read generated Protobuf descriptor set")?;
    let descriptors = FileDescriptorSet::decode(bytes.as_slice())
        .context("failed to decode generated Protobuf descriptor set")?;
    let by_name = descriptors
        .file
        .into_iter()
        .filter_map(|file| file.name.clone().map(|name| (name, file)))
        .collect::<BTreeMap<_, _>>();

    let mut inventory = Vec::new();
    for relative_path in relative_paths {
        let file = by_name.get(*relative_path).ok_or_else(|| {
            anyhow::anyhow!(
                "protoc descriptor set did not contain requested contract {relative_path}"
            )
        })?;
        inventory.push(file_inventory(relative_path, file)?);
    }
    inventory.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(inventory)
}

fn file_inventory(path: &str, file: &FileDescriptorProto) -> Result<ProtoFileInventory> {
    let mut services = file
        .service
        .iter()
        .map(|service| {
            let mut methods = service
                .method
                .iter()
                .map(|method| ProtoMethod {
                    name: method.name.clone().unwrap_or_default(),
                    input_type: method.input_type.clone().unwrap_or_default(),
                    output_type: method.output_type.clone().unwrap_or_default(),
                    client_streaming: method.client_streaming.unwrap_or(false),
                    server_streaming: method.server_streaming.unwrap_or(false),
                })
                .collect::<Vec<_>>();
            methods.sort_by(|left, right| left.name.cmp(&right.name));
            ProtoService {
                name: service.name.clone().unwrap_or_default(),
                methods,
            }
        })
        .collect::<Vec<_>>();
    services.sort_by(|left, right| left.name.cmp(&right.name));

    let mut messages = Vec::new();
    let mut enums = file
        .enum_type
        .iter()
        .map(enum_inventory)
        .collect::<Vec<_>>();
    for message in &file.message_type {
        collect_message(message, "", &mut messages, &mut enums);
    }
    messages.sort_by(|left, right| left.name.cmp(&right.name));
    enums.sort_by(|left, right| left.name.cmp(&right.name));

    let package = file.package.clone().unwrap_or_default();
    let descriptor_digest =
        normalized_descriptor_digest(path, &package, &services, &messages, &enums)?;
    Ok(ProtoFileInventory {
        path: path.to_string(),
        package,
        descriptor_digest,
        services,
        messages,
        enums,
    })
}

fn normalized_descriptor_digest(
    path: &str,
    package: &str,
    services: &[ProtoService],
    messages: &[ProtoMessage],
    enums: &[ProtoEnum],
) -> Result<String> {
    #[derive(Serialize)]
    struct NormalizedDescriptor<'a> {
        path: &'a str,
        package: &'a str,
        services: &'a [ProtoService],
        messages: &'a [ProtoMessage],
        enums: &'a [ProtoEnum],
    }
    let bytes = serde_json::to_vec(&NormalizedDescriptor {
        path,
        package,
        services,
        messages,
        enums,
    })
    .context("failed to serialize normalized Protobuf descriptor")?;
    Ok(sha256(&bytes))
}

fn collect_message(
    message: &DescriptorProto,
    parent: &str,
    messages: &mut Vec<ProtoMessage>,
    enums: &mut Vec<ProtoEnum>,
) {
    let local_name = message.name.clone().unwrap_or_default();
    let name = if parent.is_empty() {
        local_name
    } else {
        format!("{parent}.{local_name}")
    };
    let oneofs = message
        .oneof_decl
        .iter()
        .map(|oneof| oneof.name.clone().unwrap_or_default())
        .collect::<Vec<_>>();
    let mut fields = message
        .field
        .iter()
        .map(|field| ProtoField {
            name: field.name.clone().unwrap_or_default(),
            number: field.number.unwrap_or_default(),
            label: Label::try_from(field.label.unwrap_or_default())
                .map(|label| label.as_str_name().to_string())
                .unwrap_or_else(|_| "LABEL_UNKNOWN".to_string()),
            field_type: Type::try_from(field.r#type.unwrap_or_default())
                .map(|field_type| field_type.as_str_name().to_string())
                .unwrap_or_else(|_| "TYPE_UNKNOWN".to_string()),
            type_name: field.type_name.clone().filter(|value| !value.is_empty()),
            oneof: field
                .oneof_index
                .and_then(|index| usize::try_from(index).ok())
                .and_then(|index| oneofs.get(index).cloned()),
        })
        .collect::<Vec<_>>();
    fields.sort_by_key(|field| field.number);
    messages.push(ProtoMessage {
        name: name.clone(),
        fields,
    });

    for nested in &message.nested_type {
        collect_message(nested, &name, messages, enums);
    }
    for nested in &message.enum_type {
        let mut inventory = enum_inventory(nested);
        inventory.name = format!("{name}.{}", inventory.name);
        enums.push(inventory);
    }
}

fn enum_inventory(enumeration: &EnumDescriptorProto) -> ProtoEnum {
    let mut values = enumeration
        .value
        .iter()
        .map(|value| ProtoEnumValue {
            name: value.name.clone().unwrap_or_default(),
            number: value.number.unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    values.sort_by_key(|value| value.number);
    ProtoEnum {
        name: enumeration.name.clone().unwrap_or_default(),
        values,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_inventory_preserves_streaming_and_oneofs() {
        let file = FileDescriptorProto {
            name: Some("process.proto".to_string()),
            package: Some("process".to_string()),
            service: vec![prost_types::ServiceDescriptorProto {
                name: Some("Process".to_string()),
                method: vec![prost_types::MethodDescriptorProto {
                    name: Some("Start".to_string()),
                    input_type: Some(".process.StartRequest".to_string()),
                    output_type: Some(".process.StartResponse".to_string()),
                    server_streaming: Some(true),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let inventory = file_inventory("process.proto", &file).expect("build inventory");
        assert!(inventory.services[0].methods[0].server_streaming);
        assert!(inventory.descriptor_digest.starts_with("sha256:"));
    }
}
