fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // The derived clap command tree is large enough to exhaust the Windows
    // MSVC linker's 1 MiB default main-thread stack in unoptimized builds.
    // Match the conventional 8 MiB Unix main-thread reserve. This reserves
    // address space only; physical pages are committed on demand.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg-bin=a3s-box=/STACK:8388608");
    }
}
