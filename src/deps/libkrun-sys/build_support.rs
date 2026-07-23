use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Calculate a lowercase SHA-256 digest without relying on platform commands.
pub(crate) fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// Resolve the Cargo home used by the Cargo process launched from libkrun's
/// Makefile.
///
/// The outer Cargo keeps a shared package-cache lock while build scripts run.
/// Cargo does not reliably export its own `CARGO_HOME` to build scripts, so a
/// configurable override cannot be checked safely against the outer lock
/// domain. Always keep the nested cache next to this build script's outputs.
pub(crate) fn nested_cargo_home(install_dir: &Path) -> PathBuf {
    install_dir
        .parent()
        .unwrap_or(install_dir)
        .join("libkrun-cargo-home")
}
