use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};

use crate::model::{PackageExports, PublicExportInventory, SourceLock};

pub(crate) fn read_public_exports(
    root: &Path,
    source_lock: &SourceLock,
) -> Result<PublicExportInventory> {
    let e2b_source = source_lock
        .sources
        .get("e2b")
        .context("upstream lock is missing the e2b source")?;
    let interpreter_source = source_lock
        .sources
        .get("code-interpreter")
        .context("upstream lock is missing the code-interpreter source")?;

    let python_e2b_source = read(root.join("spec/e2b/public-exports/python-init.py"))?;
    let python_e2b_symbols = parse_python_all(&python_e2b_source);
    let python_e2b = PackageExports {
        language: "python".to_string(),
        package: "e2b".to_string(),
        version: package_version(e2b_source, "python")?,
        symbols: python_e2b_symbols.clone(),
        type_only_symbols: Vec::new(),
        reexports: Vec::new(),
        has_default_export: false,
    };

    let mut typescript_e2b = parse_typescript_exports(&read(
        root.join("spec/e2b/public-exports/typescript-index.ts"),
    )?);
    let template_exports = parse_typescript_exports(&read(
        root.join("spec/e2b/public-exports/typescript-template-index.ts"),
    )?);
    merge_typescript_exports(&mut typescript_e2b, &template_exports);
    let typescript_e2b = PackageExports {
        language: "typescript".to_string(),
        package: "e2b".to_string(),
        version: package_version(e2b_source, "typescript")?,
        symbols: typescript_e2b.symbols,
        type_only_symbols: typescript_e2b.type_only_symbols,
        reexports: typescript_e2b.reexports,
        has_default_export: typescript_e2b.has_default_export,
    };

    let python_interpreter_source =
        read(root.join("spec/code-interpreter/public-exports/python-init.py"))?;
    let (explicit_python_interpreter, python_reexports) =
        parse_python_import_exports(&python_interpreter_source);
    let python_interpreter_symbols = python_e2b_symbols
        .iter()
        .chain(explicit_python_interpreter.iter())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let python_interpreter = PackageExports {
        language: "python".to_string(),
        package: "e2b-code-interpreter".to_string(),
        version: package_version(interpreter_source, "python")?,
        symbols: python_interpreter_symbols,
        type_only_symbols: Vec::new(),
        reexports: python_reexports,
        has_default_export: false,
    };

    let mut typescript_interpreter = parse_typescript_exports(&read(
        root.join("spec/code-interpreter/public-exports/typescript-index.ts"),
    )?);
    typescript_interpreter
        .symbols
        .extend(typescript_e2b.symbols.iter().cloned());
    typescript_interpreter
        .type_only_symbols
        .extend(typescript_e2b.type_only_symbols.iter().cloned());
    typescript_interpreter.normalize();
    let typescript_interpreter = PackageExports {
        language: "typescript".to_string(),
        package: "@e2b/code-interpreter".to_string(),
        version: package_version(interpreter_source, "typescript")?,
        symbols: typescript_interpreter.symbols,
        type_only_symbols: typescript_interpreter.type_only_symbols,
        reexports: typescript_interpreter.reexports,
        has_default_export: typescript_interpreter.has_default_export,
    };

    let packages = BTreeMap::from([
        ("python-code-interpreter".to_string(), python_interpreter),
        ("python-e2b".to_string(), python_e2b),
        (
            "typescript-code-interpreter".to_string(),
            typescript_interpreter,
        ),
        ("typescript-e2b".to_string(), typescript_e2b),
    ]);
    Ok(PublicExportInventory {
        schema_version: 1,
        compatibility_id: source_lock.compatibility.id.clone(),
        packages,
    })
}

fn package_version(source: &crate::model::UpstreamSource, language: &str) -> Result<String> {
    source
        .packages
        .get(language)
        .cloned()
        .with_context(|| format!("upstream source is missing the {language} package version"))
}

fn read(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    std::fs::read_to_string(path)
        .with_context(|| format!("failed to read public export source {}", path.display()))
}

fn parse_python_all(source: &str) -> Vec<String> {
    let Some(start) = source.find("__all__ = [") else {
        return Vec::new();
    };
    let remainder = &source[start + "__all__ = [".len()..];
    let Some(end) = remainder.find(']') else {
        return Vec::new();
    };
    quoted_symbols(&remainder[..end])
}

fn parse_python_import_exports(source: &str) -> (Vec<String>, Vec<String>) {
    let mut symbols = BTreeSet::new();
    let mut reexports = BTreeSet::new();
    let mut multiline_target: Option<String> = None;
    for raw_line in source.lines() {
        let line = raw_line.trim();
        if multiline_target.is_some() {
            if line == ")" {
                multiline_target = None;
                continue;
            }
            for symbol in line.trim_end_matches(',').split(',') {
                insert_python_symbol(&mut symbols, symbol);
            }
            continue;
        }
        let Some(rest) = line.strip_prefix("from ") else {
            continue;
        };
        let Some((module, imported)) = rest.split_once(" import ") else {
            continue;
        };
        if imported == "*" {
            reexports.insert(module.to_string());
        } else if imported == "(" {
            multiline_target = Some(module.to_string());
        } else {
            for symbol in imported.split(',') {
                insert_python_symbol(&mut symbols, symbol);
            }
        }
    }
    (
        symbols.into_iter().collect(),
        reexports.into_iter().collect(),
    )
}

fn insert_python_symbol(symbols: &mut BTreeSet<String>, value: &str) {
    let value = value.trim().trim_end_matches(',');
    if value.is_empty() || value.starts_with('_') {
        return;
    }
    let exported = value
        .split_once(" as ")
        .map(|(_, alias)| alias)
        .unwrap_or(value);
    symbols.insert(exported.to_string());
}

fn quoted_symbols(value: &str) -> Vec<String> {
    let mut symbols = BTreeSet::new();
    for line in value.lines() {
        let line = line.trim().trim_end_matches(',').trim();
        if line.len() >= 2
            && ((line.starts_with('"') && line.ends_with('"'))
                || (line.starts_with('\'') && line.ends_with('\'')))
        {
            symbols.insert(line[1..line.len() - 1].to_string());
        }
    }
    symbols.into_iter().collect()
}

#[derive(Default)]
struct TypeScriptExports {
    symbols: Vec<String>,
    type_only_symbols: Vec<String>,
    reexports: Vec<String>,
    has_default_export: bool,
}

impl TypeScriptExports {
    fn normalize(&mut self) {
        self.symbols.sort();
        self.symbols.dedup();
        self.type_only_symbols.sort();
        self.type_only_symbols.dedup();
        self.reexports.sort();
        self.reexports.dedup();
    }
}

fn parse_typescript_exports(source: &str) -> TypeScriptExports {
    let mut exports = TypeScriptExports::default();
    let mut block: Option<(bool, String)> = None;
    for raw_line in source.lines() {
        let line = strip_typescript_comment(raw_line).trim();
        if let Some((type_only, content)) = block.as_mut() {
            content.push(' ');
            content.push_str(line);
            if line.contains('}') {
                parse_typescript_export_block(content, *type_only, &mut exports);
                block = None;
            }
            continue;
        }
        if line.starts_with("export default ") {
            exports.has_default_export = true;
            continue;
        }
        if let Some(target) = parse_star_reexport(line) {
            exports.reexports.push(target);
            continue;
        }
        if line.starts_with("export type {") || line.starts_with("export {") {
            let type_only = line.starts_with("export type {");
            if line.contains('}') {
                parse_typescript_export_block(line, type_only, &mut exports);
            } else {
                block = Some((type_only, line.to_string()));
            }
            continue;
        }
        parse_typescript_declaration(line, &mut exports);
    }
    exports.normalize();
    exports
}

fn parse_typescript_export_block(
    block: &str,
    block_type_only: bool,
    exports: &mut TypeScriptExports,
) {
    let Some(open) = block.find('{') else {
        return;
    };
    let Some(close) = block.rfind('}') else {
        return;
    };
    for item in block[open + 1..close].split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let item_type_only = block_type_only || item.starts_with("type ");
        let item = item.strip_prefix("type ").unwrap_or(item).trim();
        let exported = item
            .split_once(" as ")
            .map(|(_, alias)| alias.trim())
            .unwrap_or(item);
        if exported.is_empty() {
            continue;
        }
        if item_type_only {
            exports.type_only_symbols.push(exported.to_string());
        } else {
            exports.symbols.push(exported.to_string());
        }
    }
}

fn parse_typescript_declaration(line: &str, exports: &mut TypeScriptExports) {
    let declarations = [
        ("export abstract class ", false),
        ("export async function ", false),
        ("export class ", false),
        ("export function ", false),
        ("export const ", false),
        ("export let ", false),
        ("export enum ", false),
        ("export interface ", true),
        ("export type ", true),
    ];
    for (prefix, type_only) in declarations {
        let Some(remainder) = line.strip_prefix(prefix) else {
            continue;
        };
        let name = remainder
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .next()
            .unwrap_or_default();
        if !name.is_empty() {
            if type_only {
                exports.type_only_symbols.push(name.to_string());
            } else {
                exports.symbols.push(name.to_string());
            }
        }
        return;
    }
}

fn parse_star_reexport(line: &str) -> Option<String> {
    let rest = line.strip_prefix("export * from ")?.trim();
    let target = rest.trim_end_matches(';').trim();
    if target.len() < 2 {
        return None;
    }
    Some(target[1..target.len() - 1].to_string())
}

fn strip_typescript_comment(line: &str) -> &str {
    line.split_once("//").map(|(code, _)| code).unwrap_or(line)
}

fn merge_typescript_exports(target: &mut TypeScriptExports, source: &TypeScriptExports) {
    target.symbols.extend(source.symbols.iter().cloned());
    target
        .type_only_symbols
        .extend(source.type_only_symbols.iter().cloned());
    target.reexports.extend(source.reexports.iter().cloned());
    target.has_default_export |= source.has_default_export;
    target.normalize();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_python_all_and_import_reexports() {
        assert_eq!(
            parse_python_all("__all__ = [\n  \"Sandbox\",\n  \"AsyncSandbox\",\n]"),
            ["AsyncSandbox", "Sandbox"]
        );
        let (symbols, reexports) = parse_python_import_exports(
            "from e2b import *\nfrom .models import (\n Result,\n Logs,\n)\n",
        );
        assert_eq!(symbols, ["Logs", "Result"]);
        assert_eq!(reexports, ["e2b"]);
    }

    #[test]
    fn parses_typescript_value_type_and_star_exports() {
        let exports = parse_typescript_exports(
            "export { Sandbox, type Opts } from './sandbox'\n\
             export type { Result } from './result'\n\
             export * from 'e2b'\n\
             export default Sandbox\n",
        );
        assert_eq!(exports.symbols, ["Sandbox"]);
        assert_eq!(exports.type_only_symbols, ["Opts", "Result"]);
        assert_eq!(exports.reexports, ["e2b"]);
        assert!(exports.has_default_export);
    }
}
