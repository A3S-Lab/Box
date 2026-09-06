//! Stage already-signed macOS runtime assets without changing their identities.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

pub fn stage_runtime_dylibs(source_dir: &Path, destination_dir: &Path) -> io::Result<()> {
    println!("cargo:rerun-if-changed={}", source_dir.display());
    let mut entries = fs::read_dir(source_dir)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if !name_text.ends_with(".dylib")
            || !(name_text.starts_with("libkrun.") || name_text.starts_with("libkrunfw."))
        {
            continue;
        }
        let source = entry.path();
        let destination = destination_dir.join(&name);
        let source_metadata = fs::metadata(&source)?;
        if !source_metadata.is_file() {
            continue;
        }
        let same_file = fs::metadata(&destination).is_ok_and(|metadata| {
            metadata.dev() == source_metadata.dev() && metadata.ino() == source_metadata.ino()
        });
        if !same_file {
            // libkrun-sys already fixes @rpath install names and signs each
            // dylib. Rewriting an alias ID with install_name_tool invalidates
            // that signature. Preserve the exact bytes, including SONAMEs,
            // and include fully versioned files rather than guessing the ABI.
            fs::copy(&source, &destination)?;
        }
        println!("cargo:rerun-if-changed={}", source.display());
        println!("cargo:warning=staged {}", destination.display());
    }
    Ok(())
}
