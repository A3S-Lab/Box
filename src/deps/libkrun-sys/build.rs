// Allow unused code - these are used conditionally based on platform and build mode
#![allow(dead_code)]

mod build_support;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// ============================================================================
// Prebuilt libkrunfw configuration
// Using boxlite-ai/libkrunfw releases (fork with prebuilt releases)
// ============================================================================

// macOS: Download prebuilt kernel.c, compile locally to .dylib
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const LIBKRUNFW_PREBUILT_URL: &str =
    "https://github.com/boxlite-ai/libkrunfw/releases/download/v5.3.0/libkrunfw-prebuilt-aarch64.tgz";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const LIBKRUNFW_SHA256: &str = "12b9401d7735d1682450e4d025273c5016ec2237dcbfb76b2f0a152be6e606d6";

// Linux x86_64: Download pre-compiled .so directly (no build needed)
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const LIBKRUNFW_SO_URL: &str =
    "https://github.com/boxlite-ai/libkrunfw/releases/download/v5.3.0/libkrunfw-x86_64.tgz";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const LIBKRUNFW_SHA256: &str = "0a7bb64a35a273b8501801dd69b75736a8c676aa21aa62fb5642842cda9dc91d";

// Linux aarch64: Download pre-compiled .so directly (no build needed)
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const LIBKRUNFW_SO_URL: &str =
    "https://github.com/boxlite-ai/libkrunfw/releases/download/v5.3.0/libkrunfw-aarch64.tgz";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const LIBKRUNFW_SHA256: &str = "8b5b9211da5445d9301dafb2201431f4392ab96455512bce63a5cfbd33c49839";

// libkrun build features (NET=1 BLK=1 enables network and block device support)
// Note: TEE support (krun_set_tee_config_file) is loaded via dlsym at runtime
// since the `tee` feature in libkrun requires amd-sev or tdx to compile properly.
const LIBKRUN_BUILD_FEATURES: &[(&str, &str)] = &[("NET", "1"), ("BLK", "1")];

fn target_os() -> String {
    env::var("CARGO_CFG_TARGET_OS").unwrap_or_default()
}

// Library directory name differs by platform
#[cfg(target_os = "macos")]
const LIB_DIR: &str = "lib";
#[cfg(target_os = "linux")]
const LIB_DIR: &str = "lib64";
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const LIB_DIR: &str = "lib";

fn main() {
    // Rebuild if vendored sources change
    println!("cargo:rerun-if-changed=vendor/libkrun");
    println!("cargo:rerun-if-env-changed=A3S_DEPS_STUB");
    // Re-evaluate the system-vs-vendored decision when the toggle changes.
    println!("cargo:rerun-if-env-changed=A3S_BUILD_LIBKRUN");
    println!("cargo:rerun-if-env-changed=A3S_USE_SYSTEM_LIBKRUN");
    println!("cargo:rerun-if-env-changed=A3S_LIBKRUNFW_DYLIB");

    // Check for stub mode (for CI linting without building)
    // Set A3S_DEPS_STUB=1 to skip building and emit stub link directives
    if env::var("A3S_DEPS_STUB").is_ok() {
        println!("cargo:warning=A3S_DEPS_STUB mode: skipping libkrun build");
        println!("cargo:rustc-link-lib=dylib=krun");
        println!("cargo:LIBKRUN_A3S_DEP=/nonexistent");
        println!("cargo:LIBKRUNFW_A3S_DEP=/nonexistent");
        return;
    }

    #[cfg(target_os = "windows")]
    {
        build_windows();
    }

    #[cfg(not(target_os = "windows"))]
    {
        // The vendored macOS libkrun carries required TSI flow-control and
        // reverse-proxy fixes newer than the 1.17.0 library shipped by older
        // A3S Box formulae. Silently preferring that system dylib produces TCP
        // listeners that accept connections but never move application data.
        // Keep system linking as an explicit developer escape hatch only.
        #[cfg(target_os = "macos")]
        let force_vendored = env::var("A3S_USE_SYSTEM_LIBKRUN").is_err();
        #[cfg(not(target_os = "macos"))]
        let force_vendored = false;

        // Try to find system-installed libkrun first (unless A3S_BUILD_LIBKRUN is set)
        if env::var("A3S_BUILD_LIBKRUN").is_err() && !force_vendored {
            if let Ok(lib_dir) = find_system_libkrun() {
                println!(
                    "cargo:warning=Using system-installed libkrun from {}",
                    lib_dir.display()
                );
                configure_linking(&lib_dir, &lib_dir);
                return;
            }
            if let Some((libkrun_dir, libkrunfw_dir)) = find_cached_libkrun() {
                println!(
                    "cargo:warning=Using cached libkrun from {} and libkrunfw from {}",
                    libkrun_dir.display(),
                    libkrunfw_dir.display()
                );
                configure_linking(&libkrun_dir, &libkrunfw_dir);
                return;
            }
        } else {
            println!("cargo:warning=A3S_BUILD_LIBKRUN set: forcing build from source");
        }

        // Fall back to building from source (with prebuilt libkrunfw)
        build();
    }
}

fn find_cached_libkrun() -> Option<(PathBuf, PathBuf)> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").ok()?);
    let workspace_root = manifest_dir.join("../..").canonicalize().ok()?;
    let target_roots = candidate_target_roots(&workspace_root);

    let mut libkrun_candidates = vec![
        workspace_root.join("deps/libkrun-sys/vendor/libkrun/target/release"),
        workspace_root.join("deps/libkrun-sys/vendor/libkrun/target/release/deps"),
        workspace_root.join("target/release"),
        workspace_root.join("target/release/deps"),
        workspace_root.join("target/debug"),
        workspace_root.join("target/debug/deps"),
    ];
    for target_root in &target_roots {
        libkrun_candidates.push(target_root.join("release"));
        libkrun_candidates.push(target_root.join("release/deps"));
        libkrun_candidates.push(target_root.join("debug"));
        libkrun_candidates.push(target_root.join("debug/deps"));
    }

    let libkrun_dir = libkrun_candidates
        .into_iter()
        .find(|dir| has_library(dir, "libkrun"))?;
    let libkrunfw_dir = target_roots
        .into_iter()
        .find_map(find_libkrunfw_under_target)?;
    #[cfg(target_os = "macos")]
    ensure_macos_lib_alias(&libkrun_dir, "libkrun.dylib", "libkrun.1.dylib");
    Some((libkrun_dir, libkrunfw_dir))
}

fn candidate_target_roots(workspace_root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Ok(out_dir) = env::var("OUT_DIR") {
        let out_dir = PathBuf::from(out_dir);
        if let Some(target_root) = target_root_from_out_dir(&out_dir) {
            roots.push(target_root);
        }
    }

    if let Some(target_dir) = env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) {
        roots.push(target_dir);
    }

    roots.push(workspace_root.join("target"));

    let mut unique = Vec::new();
    for root in roots {
        if root.exists() && !unique.iter().any(|existing: &PathBuf| existing == &root) {
            unique.push(root);
        }
    }
    unique
}

fn target_root_from_out_dir(out_dir: &Path) -> Option<PathBuf> {
    for ancestor in out_dir.ancestors() {
        if ancestor.file_name().is_some_and(|name| name == "build") {
            return ancestor.parent()?.parent().map(Path::to_path_buf);
        }
    }
    None
}

fn find_sibling_libkrun_windows(triple: &str) -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").ok()?);
    let repo_root = manifest_dir.join("../../../../..").canonicalize().ok()?;
    let sibling_target = repo_root
        .parent()?
        .join("libkrun")
        .join("target")
        .join(triple);

    for profile in ["release", "debug"] {
        let candidate = sibling_target.join(profile);
        if has_library(&candidate, "krun") && candidate.join("libkrunfw.dll").exists() {
            return Some(candidate);
        }
    }

    None
}

fn find_libkrunfw_under_target(target_root: PathBuf) -> Option<PathBuf> {
    let direct_candidates = [
        target_root.join("release/build"),
        target_root.join("debug/build"),
        target_root.join("release"),
        target_root.join("debug"),
    ];

    for candidate in direct_candidates {
        if let Some(dir) = find_libkrunfw_dir(&candidate) {
            return Some(dir);
        }
    }

    None
}

fn find_libkrunfw_dir(root: &Path) -> Option<PathBuf> {
    if has_library(root, "libkrunfw") {
        return Some(root.to_path_buf());
    }

    for entry in fs::read_dir(root).ok()? {
        let path = entry.ok()?.path();
        if !path.is_dir() {
            continue;
        }
        let candidate = path.join("out").join("libkrunfw").join(LIB_DIR);
        if has_library(&candidate, "libkrunfw") {
            return Some(candidate);
        }
    }

    None
}

#[cfg(target_os = "macos")]
fn ensure_macos_lib_alias(dir: &Path, source: &str, alias: &str) {
    let source_path = dir.join(source);
    let alias_path = dir.join(alias);
    if !source_path.exists() || alias_path.exists() {
        return;
    }
    std::os::unix::fs::symlink(source, &alias_path).ok();
}

/// Try to find system-installed libkrun via pkg-config or common paths.
fn find_system_libkrun() -> Result<PathBuf, String> {
    if let Ok(lib) = pkg_config::Config::new()
        .atleast_version("1.0")
        .probe("libkrun")
    {
        // Some distributions (notably Homebrew's `libkrun-efi` formula on
        // macOS) ship a misleading `libkrun.pc` whose libdir points at a
        // directory that only contains `libkrun-efi.dylib`, not the bare
        // `libkrun.dylib` the linker looks for via `-lkrun`. Validate the
        // path before trusting it; otherwise fall through to common paths.
        if let Some(path) = lib.link_paths.iter().find(|p| has_exact_library(p, "krun")) {
            return Ok(path.clone());
        }
    }

    #[cfg(target_os = "macos")]
    let common_paths = ["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"];
    #[cfg(not(target_os = "macos"))]
    let common_paths = ["/usr/local/lib", "/usr/lib", "/usr/lib64"];

    for path in common_paths {
        let lib_path = Path::new(path);
        if has_exact_library(lib_path, "krun") {
            return Ok(lib_path.to_path_buf());
        }
    }

    Err("libkrun not found in system paths".to_string())
}

/// Checks if `dir` contains a library named exactly `lib<name>.<ext>`.
/// This is stricter than `has_library`: it prevents matching sibling
/// libraries like `libkrun-efi.dylib` when looking for `libkrun.dylib`.
/// Both unversioned (`libkrun.dylib`) and versioned (`libkrun.so.1`)
/// library names are accepted.
fn has_exact_library(dir: &Path, name: &str) -> bool {
    let extensions: &[&str] = if cfg!(target_os = "macos") {
        &["dylib"]
    } else if cfg!(target_os = "linux") {
        &["so"]
    } else {
        &["dll"]
    };

    let prefix = format!("lib{name}");
    dir.read_dir()
        .ok()
        .map(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                let filename = entry.file_name();
                let filename_str = filename.to_string_lossy();
                let Some(rest) = filename_str.strip_prefix(&prefix) else {
                    return false;
                };
                // Accept if rest equals extension (unversioned) or starts with
                // '.' and ends with extension (versioned, e.g. libkrun.so.1)
                extensions
                    .iter()
                    .any(|ext| rest == *ext || (rest.starts_with('.') && rest.ends_with(ext)))
            })
        })
        .unwrap_or(false)
}

/// Returns libkrun build environment with features enabled.
fn libkrun_build_env(libkrunfw_install: &Path) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert(
        "PKG_CONFIG_PATH".to_string(),
        format!("{}/{}/pkgconfig", libkrunfw_install.display(), LIB_DIR),
    );
    for (key, value) in LIBKRUN_BUILD_FEATURES {
        env.insert(key.to_string(), value.to_string());
    }
    env
}

/// Verifies vendored libkrun source exists.
fn verify_libkrun_source(manifest_dir: &Path) {
    let libkrun_src = manifest_dir.join("vendor/libkrun");
    if !libkrun_src.exists() {
        eprintln!("ERROR: Vendored libkrun source not found");
        eprintln!();
        eprintln!("Initialize git submodule:");
        eprintln!("  git submodule update --init vendor/libkrun");
        std::process::exit(1);
    }
}

/// Runs a command and panics with a helpful message if it fails.
fn run_command(cmd: &mut Command, description: &str) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("Failed to execute {}: {}", description, e));

    if !status.success() {
        panic!("{} failed with exit code: {:?}", description, status.code());
    }
}

/// Checks if a directory contains any library file matching the given prefix.
fn has_library(dir: &Path, prefix: &str) -> bool {
    let extensions = if cfg!(target_os = "macos") {
        vec!["dylib"]
    } else if cfg!(target_os = "linux") {
        vec!["so"]
    } else {
        vec!["dll"]
    };

    dir.read_dir()
        .ok()
        .map(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                let filename = entry.file_name().to_string_lossy().to_string();
                filename.starts_with(prefix)
                    && extensions
                        .iter()
                        .any(|ext| entry.path().extension().is_some_and(|e| e == *ext))
            })
        })
        .unwrap_or(false)
}

/// Creates a make command with common configuration.
fn make_command(
    source_dir: &Path,
    install_dir: &Path,
    extra_env: &HashMap<String, String>,
) -> Command {
    let cargo_home = build_support::nested_cargo_home(install_dir);
    fs::create_dir_all(&cargo_home).unwrap_or_else(|error| {
        panic!(
            "Failed to create nested libkrun Cargo home {}: {}",
            cargo_home.display(),
            error
        )
    });

    let mut cmd = Command::new("make");
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    cmd.args(["-j", &num_cpus::get().to_string()])
        .arg("MAKEFLAGS=")
        // libkrun's Makefile invokes a nested Cargo build. Do not leak the
        // outer workspace's clippy wrapper or lint flags into that independent
        // vendored workspace: `cargo clippy -- -D warnings` must lint A3S code,
        // not turn upstream warnings into a build-script failure.
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CLIPPY_ARGS")
        // The outer Cargo holds a shared lock in its package cache throughout
        // this build script. libkrun's Makefile launches Cargo again, and that
        // process can wait forever when it tries to upgrade the same cache to
        // an exclusive mutation lock. An isolated Cargo home gives the nested
        // process an independent lock domain. Also keep an outer target-dir
        // override from moving artifacts away from paths expected by Make.
        .env("CARGO_HOME", cargo_home)
        .env_remove("CARGO_TARGET_DIR")
        .env("PREFIX", install_dir)
        .current_dir(source_dir);

    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    cmd
}

/// Builds a library using Make with the specified parameters.
fn build_with_make(
    source_dir: &Path,
    install_dir: &Path,
    lib_name: &str,
    extra_env: HashMap<String, String>,
) {
    println!("cargo:warning=Building {} from source...", lib_name);

    std::fs::create_dir_all(install_dir)
        .unwrap_or_else(|e| panic!("Failed to create install directory: {}", e));

    let mut make_cmd = make_command(source_dir, install_dir, &extra_env);
    run_command(&mut make_cmd, &format!("make {}", lib_name));

    let mut install_cmd = make_command(source_dir, install_dir, &extra_env);
    install_cmd.arg("install");
    run_command(&mut install_cmd, &format!("make install {}", lib_name));
}

/// Configure linking for libkrun.
fn configure_linking(libkrun_dir: &Path, libkrunfw_dir: &Path) {
    println!("cargo:rustc-link-search=native={}", libkrun_dir.display());
    println!("cargo:rustc-link-lib=dylib=krun");
    #[cfg(target_os = "macos")]
    {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", libkrun_dir.display());
        println!(
            "cargo:rustc-link-arg=-Wl,-rpath,{}",
            libkrunfw_dir.display()
        );
        stage_macos_runtime_libraries(libkrun_dir, libkrunfw_dir);
    }

    println!("cargo:LIBKRUN_A3S_DEP={}", libkrun_dir.display());
    println!("cargo:LIBKRUNFW_A3S_DEP={}", libkrunfw_dir.display());
}

#[cfg(target_os = "macos")]
fn stage_macos_runtime_libraries(libkrun_dir: &Path, libkrunfw_dir: &Path) {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is required"));
    let target_root = target_root_from_out_dir(&out_dir)
        .expect("failed to derive Cargo target root from OUT_DIR");
    let runtime_dir = target_root.join("lib");
    fs::create_dir_all(&runtime_dir).unwrap_or_else(|error| {
        panic!(
            "failed to create runtime library directory {}: {error}",
            runtime_dir.display()
        )
    });

    for source_dir in [libkrun_dir, libkrunfw_dir] {
        for entry in fs::read_dir(source_dir)
            .unwrap_or_else(|error| panic!("failed to inspect {}: {error}", source_dir.display()))
        {
            let entry = entry.expect("failed to inspect runtime library entry");
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.ends_with(".dylib")
                || !(name.starts_with("libkrun.") || name.starts_with("libkrunfw."))
            {
                continue;
            }
            let resolved = path.canonicalize().unwrap_or_else(|error| {
                panic!(
                    "failed to resolve runtime library {}: {error}",
                    path.display()
                )
            });
            let destination = runtime_dir.join(name);
            let temporary = runtime_dir.join(format!(".{name}.tmp-{}", std::process::id()));
            fs::copy(&resolved, &temporary).unwrap_or_else(|error| {
                panic!(
                    "failed to stage runtime library {} at {}: {error}",
                    resolved.display(),
                    temporary.display()
                )
            });
            fs::rename(&temporary, &destination).unwrap_or_else(|error| {
                panic!(
                    "failed to activate runtime library {} at {}: {error}",
                    temporary.display(),
                    destination.display()
                )
            });
        }
    }
    println!(
        "cargo:warning=Staged macOS runtime libraries in {}",
        runtime_dir.display()
    );
}

/// Downloads a file from URL to the specified path.
fn download_file(url: &str, dest: &Path) -> io::Result<()> {
    println!("cargo:warning=Downloading {}...", url);

    // Retry + abort-on-stall: some networks intermittently stall on large GitHub
    // release downloads, and a bare curl with no timeout hangs forever. Retry and
    // kill transfers that drop below 2KB/s for 30s so a stalled pull self-heals.
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--retry",
            "20",
            "--retry-all-errors",
            "--retry-delay",
            "3",
            "--connect-timeout",
            "20",
            "--speed-limit",
            "2048",
            "--speed-time",
            "30",
            "-o",
            dest.to_str().unwrap(),
            url,
        ])
        .output()?;

    if !output.status.success() {
        return Err(io::Error::other(format!(
            "curl failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(())
}

/// Verifies SHA256 checksum of a file.
fn verify_sha256(file: &Path, expected: &str) -> io::Result<()> {
    let actual = if cfg!(target_os = "windows") {
        // PowerShell is available on all modern Windows
        let script = format!(
            "(Get-FileHash -Algorithm SHA256 -LiteralPath '{}').Hash.ToLower()",
            file.display()
        );
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &script])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other("PowerShell Get-FileHash failed"));
        }
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        let (cmd, args): (&str, Vec<&str>) = if cfg!(target_os = "linux") {
            ("sha256sum", vec![file.to_str().unwrap()])
        } else {
            ("shasum", vec!["-a", "256", file.to_str().unwrap()])
        };
        let output = Command::new(cmd).args(&args).output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!("{} failed", cmd)));
        }
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string()
    };

    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SHA256 mismatch: expected {}, got {}", expected, actual),
        ));
    }

    println!("cargo:warning=SHA256 verified: {}", expected);
    Ok(())
}

/// Extracts a tarball to the specified directory.
fn extract_tarball(tarball: &Path, dest: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;

    let status = Command::new("tar")
        .args([
            "-xzf",
            tarball.to_str().unwrap(),
            "-C",
            dest.to_str().unwrap(),
        ])
        .status()?;

    if !status.success() {
        return Err(io::Error::other("tar extraction failed"));
    }

    Ok(())
}

/// Fixes the install_name on macOS to use @rpath.
#[cfg(target_os = "macos")]
fn fix_install_name(lib_name: &str, lib_path: &Path) {
    let status = Command::new("install_name_tool")
        .args([
            "-id",
            &format!("@rpath/{}", lib_name),
            lib_path.to_str().unwrap(),
        ])
        .status()
        .expect("Failed to execute install_name_tool");

    if !status.success() {
        panic!("Failed to set install_name for {}", lib_name);
    }
}

#[cfg(target_os = "linux")]
fn fix_install_name(lib_name: &str, lib_path: &Path) {
    let lib_path_str = lib_path.to_str().expect("Invalid library path");

    let result = Command::new("patchelf")
        .args(["--set-soname", lib_name, lib_path_str])
        .status();

    match result {
        Ok(status) if status.success() => {
            println!("cargo:warning=Fixed soname for {}", lib_name);
        }
        Ok(_) => {
            println!(
                "cargo:warning=patchelf failed for {}, continuing anyway",
                lib_name
            );
        }
        Err(_) => {
            println!(
                "cargo:warning=patchelf not found, skipping soname fix for {}",
                lib_name
            );
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn fix_install_name(_lib_name: &str, _lib_path: &Path) {}

/// Extract SONAME from versioned library filename
/// e.g., libkrunfw.so.4.9.0 -> Some("libkrunfw.so.4")
fn extract_major_soname(filename: &str) -> Option<String> {
    if let Some(so_pos) = filename.find(".so.") {
        let base = &filename[..so_pos + 3];
        let versions = &filename[so_pos + 4..];

        if let Some(major) = versions.split('.').next() {
            return Some(format!("{}.{}", base, major));
        }
    }
    None
}

// ============================================================================
// macOS-specific build functions
// ============================================================================

#[cfg(target_os = "macos")]
fn append_env_path(var: &str, path: &Path) {
    let mut paths = env::var_os(var)
        .map(|value| env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();
    if paths.iter().any(|existing| existing == path) {
        return;
    }
    paths.push(path.to_path_buf());
    if let Ok(joined) = env::join_paths(paths) {
        env::set_var(var, joined);
    }
}

#[cfg(target_os = "macos")]
fn setup_libclang_path() {
    if let Some(existing) = env::var_os("LIBCLANG_PATH").map(PathBuf::from) {
        append_env_path("DYLD_FALLBACK_LIBRARY_PATH", &existing);
        append_env_path("DYLD_LIBRARY_PATH", &existing);
        return;
    }
    if Command::new("llvm-config")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        return;
    }

    if let Ok(output) = Command::new("brew").args(["--prefix", "llvm"]).output() {
        if output.status.success() {
            let prefix = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let lib_path = format!("{}/lib", prefix);
            let lib_path = PathBuf::from(lib_path);
            if lib_path.join("libclang.dylib").exists() {
                env::set_var("LIBCLANG_PATH", &lib_path);
                append_env_path("DYLD_FALLBACK_LIBRARY_PATH", &lib_path);
                append_env_path("DYLD_LIBRARY_PATH", &lib_path);
                println!("cargo:warning=Set LIBCLANG_PATH to {}", lib_path.display());
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn fix_macos_libs(lib_dir: &Path, lib_prefix: &str) -> Result<(), String> {
    for entry in fs::read_dir(lib_dir).map_err(|e| format!("Failed to read lib dir: {}", e))? {
        let entry = entry.map_err(|e| format!("Failed to read entry: {}", e))?;
        let path = entry.path();
        let filename = path.file_name().unwrap().to_string_lossy().to_string();

        if filename.starts_with(lib_prefix) && filename.contains(".dylib") {
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| format!("Failed to get metadata: {}", e))?;

            if metadata.file_type().is_symlink() {
                continue;
            }

            fix_install_name(&filename, &path);

            let sign_status = Command::new("codesign")
                .args(["-s", "-", "--force"])
                .arg(&path)
                .status()
                .map_err(|e| format!("Failed to run codesign: {}", e))?;

            if !sign_status.success() {
                return Err(format!("codesign failed for {}", filename));
            }

            println!("cargo:warning=Fixed and signed {}", filename);
        }
    }

    Ok(())
}

/// Downloads and extracts the prebuilt libkrunfw tarball (macOS).
#[cfg(target_os = "macos")]
fn download_libkrunfw_prebuilt(out_dir: &Path) -> PathBuf {
    let tarball_path = out_dir.join(format!(
        "libkrunfw-prebuilt-{}.tar.gz",
        &LIBKRUNFW_SHA256[..12]
    ));
    let extract_dir = out_dir.join("libkrunfw-src");
    let src_dir = extract_dir.join("libkrunfw");
    let marker = extract_dir.join(".source-sha256");

    if src_dir.join("kernel.c").exists()
        && fs::read_to_string(&marker).is_ok_and(|value| value == LIBKRUNFW_SHA256)
    {
        println!("cargo:warning=Using cached libkrunfw source");
        return src_dir;
    }

    if !tarball_path.exists() {
        download_file(LIBKRUNFW_PREBUILT_URL, &tarball_path)
            .unwrap_or_else(|e| panic!("Failed to download libkrunfw: {}", e));

        verify_sha256(&tarball_path, LIBKRUNFW_SHA256)
            .unwrap_or_else(|e| panic!("Failed to verify libkrunfw checksum: {}", e));
    }

    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir).ok();
    }
    extract_tarball(&tarball_path, &extract_dir)
        .unwrap_or_else(|e| panic!("Failed to extract libkrunfw: {}", e));
    fs::write(&marker, LIBKRUNFW_SHA256)
        .unwrap_or_else(|e| panic!("Failed to record libkrunfw source digest: {}", e));

    println!("cargo:warning=Extracted libkrunfw to {}", src_dir.display());
    src_dir
}

// Windows: prebuilt krun.dll + krun.lib + libkrunfw.dll bundle
const KRUN_WINDOWS_URL: &str =
    "https://github.com/A3S-Lab/libkrun/releases/download/v1.17.5-windows/krun-windows-x64.zip";
const KRUN_WINDOWS_SHA256: &str =
    "8d5bd3d2452bb6e36973b8b57d34e16fdee0af677d32f883aa689f22708dbc2a";

/// macOS: Build libkrun from source, use prebuilt libkrunfw
#[cfg(target_os = "macos")]
fn build() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let libkrunfw_install = out_dir.join("libkrunfw");
    let libkrun_install = out_dir.join("libkrun");
    let libkrunfw_lib = libkrunfw_install.join(LIB_DIR);
    let libkrun_lib = libkrun_install.join(LIB_DIR);
    let firmware_marker = out_dir.join("libkrunfw-src/.source-sha256");
    let firmware_current =
        fs::read_to_string(&firmware_marker).is_ok_and(|value| value == LIBKRUNFW_SHA256);

    // Skip build if outputs already exist (incremental build optimization)
    if env::var("A3S_BUILD_LIBKRUN").is_err()
        && firmware_current
        && has_library(&libkrunfw_lib, "libkrunfw")
        && has_library(&libkrun_lib, "libkrun")
    {
        configure_linking(&libkrun_lib, &libkrunfw_lib);
        return;
    }
    if !firmware_current {
        let _ = fs::remove_dir_all(&libkrunfw_install);
        let _ = fs::remove_dir_all(&libkrun_install);
    }

    println!("cargo:warning=Building libkrun-sys for macOS (using prebuilt libkrunfw)");

    // Verify vendored libkrun source exists (libkrunfw is downloaded)
    verify_libkrun_source(&manifest_dir);

    let libkrun_src = manifest_dir.join("vendor/libkrun");

    // Setup LIBCLANG_PATH for bindgen if needed
    setup_libclang_path();

    // 1-2. Use an explicitly built patched firmware when supplied; otherwise
    // build the checksum-verified upstream prebuilt source bundle. The override
    // is path-only and opt-in so release builds cannot silently consume ambient
    // firmware. `firmware/build-patched-darwin-arm64.sh` produces this artifact.
    if let Ok(override_path) = env::var("A3S_LIBKRUNFW_DYLIB") {
        let override_path = PathBuf::from(override_path);
        if !override_path.is_file() {
            panic!(
                "A3S_LIBKRUNFW_DYLIB is not a file: {}",
                override_path.display()
            );
        }
        fs::create_dir_all(&libkrunfw_lib).expect("create libkrunfw override directory");
        fs::copy(&override_path, libkrunfw_lib.join("libkrunfw.5.dylib"))
            .unwrap_or_else(|error| panic!("Failed to stage patched libkrunfw: {error}"));
        println!(
            "cargo:warning=Using explicit patched libkrunfw from {}",
            override_path.display()
        );
    } else {
        let libkrunfw_src = download_libkrunfw_prebuilt(&out_dir);
        build_with_make(
            &libkrunfw_src,
            &libkrunfw_install,
            "libkrunfw",
            HashMap::new(),
        );
    }

    // 3. Build libkrun from vendored source
    build_with_make(
        &libkrun_src,
        &libkrun_install,
        "libkrun",
        libkrun_build_env(&libkrunfw_install),
    );

    // 4. Fix install names for @rpath and re-sign
    fix_macos_libs(&libkrunfw_lib, "libkrunfw")
        .unwrap_or_else(|e| panic!("Failed to fix libkrunfw: {}", e));

    fix_macos_libs(&libkrun_lib, "libkrun")
        .unwrap_or_else(|e| panic!("Failed to fix libkrun: {}", e));

    // 5. Configure linking
    configure_linking(&libkrun_lib, &libkrunfw_lib);
}

// ============================================================================
// Linux-specific build functions
// ============================================================================

#[cfg(target_os = "linux")]
fn fix_linux_libs(lib_dir: &Path, lib_prefix: &str) -> Result<(), String> {
    for entry in fs::read_dir(lib_dir).map_err(|e| format!("Failed to read directory: {}", e))? {
        let entry = entry.map_err(|e| format!("Failed to read entry: {}", e))?;
        let path = entry.path();
        let filename = path.file_name().unwrap().to_string_lossy().to_string();

        if filename.starts_with(lib_prefix) && filename.contains(".so") {
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| format!("Failed to get metadata: {}", e))?;

            if metadata.file_type().is_symlink() {
                continue;
            }

            // For libkrunfw only: rename to major version
            if lib_prefix == "libkrunfw" {
                if let Some(soname) = extract_major_soname(&filename) {
                    if soname != filename {
                        let new_path = lib_dir.join(&soname);
                        fs::rename(&path, &new_path)
                            .map_err(|e| format!("Failed to rename file: {}", e))?;
                        println!("cargo:warning=Renamed {} to {}", filename, soname);
                        fix_install_name(&soname, &new_path);
                        continue;
                    }
                }
            }

            fix_install_name(&filename, &path);
        }
    }

    Ok(())
}

/// Downloads pre-compiled libkrunfw .so files (Linux).
#[cfg(target_os = "linux")]
fn download_libkrunfw_so(install_dir: &Path) {
    let lib_dir = install_dir.join(LIB_DIR);
    let marker = install_dir.join(".source-sha256");

    if has_library(&lib_dir, "libkrunfw")
        && fs::read_to_string(&marker).is_ok_and(|value| value == LIBKRUNFW_SHA256)
    {
        println!("cargo:warning=Using cached libkrunfw.so");
        return;
    }

    if lib_dir.exists() {
        fs::remove_dir_all(&lib_dir)
            .unwrap_or_else(|e| panic!("Failed to remove stale libkrunfw cache: {}", e));
    }

    fs::create_dir_all(install_dir)
        .unwrap_or_else(|e| panic!("Failed to create install dir: {}", e));

    let tarball_path = install_dir.join(format!("libkrunfw-{}.tgz", &LIBKRUNFW_SHA256[..12]));

    if !tarball_path.exists() {
        download_file(LIBKRUNFW_SO_URL, &tarball_path)
            .unwrap_or_else(|e| panic!("Failed to download libkrunfw: {}", e));

        verify_sha256(&tarball_path, LIBKRUNFW_SHA256)
            .unwrap_or_else(|e| panic!("Failed to verify libkrunfw checksum: {}", e));
    }

    extract_tarball(&tarball_path, install_dir)
        .unwrap_or_else(|e| panic!("Failed to extract libkrunfw: {}", e));
    fs::write(&marker, LIBKRUNFW_SHA256)
        .unwrap_or_else(|e| panic!("Failed to record libkrunfw source digest: {}", e));

    println!(
        "cargo:warning=Extracted libkrunfw.so to {}",
        lib_dir.display()
    );
}

/// Windows: Download and extract the prebuilt krun-windows-x64.zip.
/// Returns the directory containing krun.lib, krun.dll, and libkrunfw.dll.
fn download_krun_windows_prebuilt(out_dir: &Path) -> PathBuf {
    let zip_path = out_dir.join("krun-windows-x64.zip");
    let extract_dir = out_dir.join("krun-windows-x64");
    let lib_dir = extract_dir.join("krun-windows-x64");

    if lib_dir.join("krun.lib").exists() {
        println!("cargo:warning=Using cached krun Windows prebuilt");
        return lib_dir;
    }

    download_file(KRUN_WINDOWS_URL, &zip_path)
        .unwrap_or_else(|e| panic!("Failed to download krun Windows prebuilt: {}", e));

    verify_sha256(&zip_path, KRUN_WINDOWS_SHA256)
        .unwrap_or_else(|e| panic!("Failed to verify krun Windows prebuilt checksum: {}", e));

    fs::create_dir_all(&extract_dir)
        .unwrap_or_else(|e| panic!("Failed to create extract dir: {}", e));

    // Use PowerShell to unzip (available on all modern Windows)
    let status = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
                zip_path.display(),
                extract_dir.display(),
            ),
        ])
        .status()
        .unwrap_or_else(|e| panic!("Failed to run PowerShell unzip: {}", e));

    assert!(status.success(), "PowerShell Expand-Archive failed");
    println!(
        "cargo:warning=Extracted krun Windows prebuilt to {}",
        lib_dir.display()
    );
    lib_dir
}

/// Windows: Download and link the prebuilt krun.dll + krun.lib bundle.
///
/// Search order:
///   1. LIBKRUN_DIR env var (local build override)
///   2. ../libkrun/target/<triple>/{release,debug} (local sibling checkout)
///   3. deps/libkrun-sys/prebuilt/x86_64-pc-windows-msvc/ (vendored)
///   4. Auto-download krun-windows-x64.zip from GitHub Releases into OUT_DIR
fn build_windows() {
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "x86_64".to_string());
    let triple = format!("{}-pc-windows-msvc", target_arch);
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let lib_dir = if let Ok(dir) = env::var("LIBKRUN_DIR") {
        PathBuf::from(dir)
    } else if let Some(dir) = find_sibling_libkrun_windows(&triple) {
        println!(
            "cargo:warning=Using sibling libkrun Windows build from {}",
            dir.display()
        );
        dir
    } else {
        let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        let prebuilt = manifest_dir.join("prebuilt").join(&triple);
        if prebuilt.join("krun.lib").exists() {
            prebuilt
        } else {
            download_krun_windows_prebuilt(&out_dir)
        }
    };

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=krun");
    println!("cargo:rustc-link-lib=WinHvPlatform");
    println!("cargo:LIBKRUN_A3S_DEP={}", lib_dir.display());
    println!("cargo:LIBKRUNFW_A3S_DEP={}", lib_dir.display());
    println!("cargo:rerun-if-env-changed=LIBKRUN_DIR");
}

/// Linux: Build libkrun from source, download prebuilt libkrunfw
#[cfg(target_os = "linux")]
fn build() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let libkrunfw_install = out_dir.join("libkrunfw");
    let libkrun_install = out_dir.join("libkrun");
    let libkrunfw_lib_dir = libkrunfw_install.join(LIB_DIR);
    let libkrun_lib_dir = libkrun_install.join(LIB_DIR);
    let firmware_current = fs::read_to_string(libkrunfw_install.join(".source-sha256"))
        .is_ok_and(|value| value == LIBKRUNFW_SHA256);

    // Skip build if outputs already exist (incremental build optimization)
    if env::var("A3S_BUILD_LIBKRUN").is_err()
        && firmware_current
        && has_library(&libkrunfw_lib_dir, "libkrunfw")
        && has_library(&libkrun_lib_dir, "libkrun")
    {
        configure_linking(&libkrun_lib_dir, &libkrunfw_lib_dir);
        return;
    }
    if !firmware_current {
        let _ = fs::remove_dir_all(&libkrun_install);
    }

    println!("cargo:warning=Building libkrun-sys for Linux (using prebuilt libkrunfw)");

    // Verify vendored libkrun source exists (libkrunfw is downloaded)
    verify_libkrun_source(&manifest_dir);

    // 1. Download pre-compiled libkrunfw.so directly (no build needed)
    download_libkrunfw_so(&libkrunfw_install);

    // 2. Build libkrun from vendored source
    let libkrun_src = manifest_dir.join("vendor/libkrun");
    build_with_make(
        &libkrun_src,
        &libkrun_install,
        "libkrun",
        libkrun_build_env(&libkrunfw_install),
    );

    // 3. Fix library names
    fix_linux_libs(&libkrun_lib_dir, "libkrun")
        .unwrap_or_else(|e| panic!("Failed to fix libkrun: {}", e));

    fix_linux_libs(&libkrunfw_lib_dir, "libkrunfw")
        .unwrap_or_else(|e| panic!("Failed to fix libkrunfw: {}", e));

    // 4. Configure linking
    configure_linking(&libkrun_lib_dir, &libkrunfw_lib_dir);
}

/// Unsupported platform
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn build() {
    eprintln!("ERROR: libkrun is only supported on macOS, Linux, and Windows");
    eprintln!();
    eprintln!("Supported platforms:");
    eprintln!("  - macOS ARM64 (Apple Silicon)");
    eprintln!("  - Linux x86_64 / aarch64");
    eprintln!("  - Windows x86_64 (WHPX)");
    std::process::exit(1);
}
