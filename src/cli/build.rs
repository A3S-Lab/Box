fn main() {
    // Read libkrun library paths from libkrun-sys build metadata.
    // These are set by the `links = "krun"` declaration in libkrun-sys.
    let libkrun_dir = std::env::var("DEP_KRUN_LIBKRUN_A3S_DEP").unwrap_or_default();
    let libkrunfw_dir = std::env::var("DEP_KRUN_LIBKRUNFW_A3S_DEP").unwrap_or_default();

    #[cfg(windows)]
    copy_runtime_dlls(&libkrun_dir, &libkrunfw_dir);

    // Emit rpath so the binary can find libkrun at runtime.
    #[cfg(not(windows))]
    if !libkrun_dir.is_empty() && libkrun_dir != "/nonexistent" {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{libkrun_dir}");
    }
    #[cfg(not(windows))]
    if !libkrunfw_dir.is_empty() && libkrunfw_dir != "/nonexistent" && libkrunfw_dir != libkrun_dir
    {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{libkrunfw_dir}");
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
