fn main() {
    // Read libkrun library paths from libkrun-sys build metadata.
    // These are set by the `links = "krun"` declaration in libkrun-sys.
    let libkrun_dir = std::env::var("DEP_KRUN_LIBKRUN_A3S_DEP").unwrap_or_default();
    let libkrunfw_dir = std::env::var("DEP_KRUN_LIBKRUNFW_A3S_DEP").unwrap_or_default();

    // Emit rpath so the binary can find libkrun at runtime.
    if !libkrun_dir.is_empty() && libkrun_dir != "/nonexistent" {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{libkrun_dir}");
    }
    if !libkrunfw_dir.is_empty() && libkrunfw_dir != "/nonexistent" && libkrunfw_dir != libkrun_dir
    {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{libkrunfw_dir}");
    }
}
