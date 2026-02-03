//! Build script for a3s-box-code
//!
//! Compiles the gRPC proto definitions from src/proto.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Compile proto file from src/proto (../proto relative to src/code)
    tonic_build::configure()
        .build_server(true) // We're the server
        .build_client(false) // No client needed
        .compile(&["../proto/code_agent.proto"], &["../proto"])?;

    println!("cargo:rerun-if-changed=../proto/code_agent.proto");

    Ok(())
}
