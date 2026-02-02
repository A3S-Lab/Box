//! Build script for a3s-box-code
//!
//! Compiles the gRPC proto definitions.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Compile proto file to OUT_DIR (standard location for generated code)
    tonic_build::configure()
        .build_server(true)  // We're the server
        .build_client(false) // No client needed
        .compile(&["proto/agent.proto"], &["proto"])?;

    println!("cargo:rerun-if-changed=proto/agent.proto");

    Ok(())
}
