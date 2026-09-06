#[cfg(target_os = "macos")]
#[path = "build_support/macos.rs"]
mod macos;

fn main() {
    // Read libkrun library paths from libkrun-sys build metadata.
    // Cargo derives the DEP_* prefix from `links = "a3s_krun"` in
    // libkrun-sys. Keep the old prefix as a fallback for older releases.
    let libkrun_dir = std::env::var("DEP_A3S_KRUN_LIBKRUN_A3S_DEP")
        .or_else(|_| std::env::var("DEP_KRUN_LIBKRUN_A3S_DEP"))
        .unwrap_or_default();
    let libkrunfw_dir = std::env::var("DEP_A3S_KRUN_LIBKRUNFW_A3S_DEP")
        .or_else(|_| std::env::var("DEP_KRUN_LIBKRUNFW_A3S_DEP"))
        .unwrap_or_default();

    #[cfg(windows)]
    copy_runtime_dlls(&libkrun_dir, &libkrunfw_dir);

    #[cfg(target_os = "macos")]
    copy_runtime_dylibs(&libkrun_dir, &libkrunfw_dir);

    // On macOS, use an rpath rooted at the installed binary's sibling `lib`
    // directory. The runtime launcher also exports that directory through
    // DYLD_LIBRARY_PATH for libkrunfw's lazily loaded dependency.
    // On Linux, emit rpath to the build directory (runtime discovery is handled differently).
    #[cfg(target_os = "macos")]
    {
        // Use @executable_path/../lib to find libkrun in the installed package.
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../lib");
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        // Linux: emit rpath so the binary can find libkrun at runtime.
        if !libkrun_dir.is_empty() && libkrun_dir != "/nonexistent" {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{libkrun_dir}");
        }
        if !libkrunfw_dir.is_empty()
            && libkrunfw_dir != "/nonexistent"
            && libkrunfw_dir != libkrun_dir
        {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{libkrunfw_dir}");
        }
    }
}

#[cfg(target_os = "macos")]
fn copy_runtime_dylibs(libkrun_dir: &str, libkrunfw_dir: &str) {
    use std::path::{Path, PathBuf};
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let bin_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("unexpected OUT_DIR depth");

    for source in [libkrun_dir, libkrunfw_dir] {
        if source.is_empty() || source == "/nonexistent" {
            continue;
        }
        macos::stage_runtime_dylibs(Path::new(source), bin_dir).unwrap_or_else(|error| {
            panic!("failed to stage runtime dylibs from {source}: {error}")
        });
    }
}

#[cfg(windows)]
fn copy_runtime_dlls(libkrun_dir: &str, libkrunfw_dir: &str) {
    use std::path::{Path, PathBuf};

    fn copy_if_present(src_dir: &str, file_name: &str, bin_dir: &Path) {
        if src_dir.is_empty() || src_dir == "/nonexistent" {
            return;
        }

        let src = PathBuf::from(src_dir).join(file_name);
        if !src.exists() {
            println!("cargo:warning={} not found at {}", file_name, src.display());
            return;
        }

        let dst = bin_dir.join(file_name);
        std::fs::copy(&src, &dst).unwrap_or_else(|e| panic!("failed to copy {}: {}", file_name, e));
        println!(
            "cargo:warning=copied {} -> {}",
            src.display(),
            dst.display()
        );
        println!("cargo:rerun-if-changed={}", src.display());
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let bin_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("unexpected OUT_DIR depth");

    copy_if_present(libkrun_dir, "krun.dll", bin_dir);
    copy_if_present(libkrunfw_dir, "libkrunfw.dll", bin_dir);
}
