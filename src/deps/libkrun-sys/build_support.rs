use std::path::{Path, PathBuf};

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
