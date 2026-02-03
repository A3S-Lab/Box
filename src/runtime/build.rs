fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Compile protobuf definitions for gRPC
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .out_dir("src/grpc/proto")
        .compile(&["proto/agent.proto"], &["proto"])?;

    Ok(())
}
