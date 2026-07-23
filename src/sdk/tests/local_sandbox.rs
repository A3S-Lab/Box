//! Opt-in real-runtime proof for the zero-configuration E2B-style Rust API.

use std::error::Error;
use std::path::PathBuf;

use a3s_box_sdk::{
    A3sBoxClient, ClientError, ExecutionIsolation, ExecutionSnapshotId, Sandbox,
    SandboxCreateOptions, SandboxNetwork,
};

type AnyError = Box<dyn Error + Send + Sync>;

#[tokio::test]
#[ignore = "requires a real local A3S Box runtime and runnable OCI image"]
async fn e2b_style_local_sandbox_runs_without_remote_credentials() -> Result<(), AnyError> {
    require(
        std::env::var("A3S_BOX_SDK_LOCAL_SMOKE").as_deref() == Ok("1"),
        "set A3S_BOX_SDK_LOCAL_SMOKE=1 to acknowledge the destructive smoke test",
    )?;
    for variable in [
        "E2B_API_KEY",
        "E2B_API_URL",
        "E2B_DOMAIN",
        "A3S_BOX_API_KEY",
        "A3S_BOX_ENDPOINT",
        "A3S_BOX_DOMAIN",
        "A3S_BOX_SANDBOX_URL",
    ] {
        require(
            std::env::var_os(variable).is_none(),
            format!("{variable} must be unset for the zero-configuration local SDK smoke"),
        )?;
    }

    let home = validated_home()?;
    let isolation = requested_isolation()?;
    let base_image =
        std::env::var("A3S_BOX_SDK_SMOKE_IMAGE").unwrap_or_else(|_| "alpine:3.20".to_string());
    let client = A3sBoxClient::from_home(&home);
    let context = home.join("rust-sdk-build-context");
    std::fs::create_dir_all(&context)?;
    std::fs::write(
        context.join("Dockerfile"),
        format!("FROM {base_image}\nENV A3S_SDK_BASE=ready\nWORKDIR /workspace\n"),
    )?;
    let image = client
        .image(&context)
        .tag("local/a3s-sdk-smoke-rust:latest")
        .build()
        .await?;
    let volume = client
        .volume("rust-sdk-cache")
        .label("purpose", "local-sdk-smoke")
        .create()?;
    let network = if isolation == ExecutionIsolation::Microvm {
        Some(
            client
                .network("rust-sdk-network")
                .subnet("10.89.91.0/24")
                .create()?,
        )
    } else {
        None
    };
    let builder = client
        .sandbox(image.reference.clone())
        .timeout_seconds(300)
        .metadata("purpose", "local-sdk-smoke")
        .isolation(isolation)
        .mount_named(&volume.name, "/cache")
        .workdir("/workspace");
    let builder = match &network {
        Some(network) => builder
            .network(SandboxNetwork::bridge(&network.name))
            .publish_tcp(0, 8080),
        None => builder.network(SandboxNetwork::Disabled),
    };
    let sandbox = builder.start().await?;
    let sandbox_id = sandbox.id().to_string();

    let exercise_result = exercise(&sandbox, &client, isolation, &image.reference).await;
    let cleanup_result = sandbox.kill().await;
    if let Err(error) = exercise_result {
        if let Err(cleanup_error) = cleanup_result {
            eprintln!("local SDK cleanup also failed: {cleanup_error}");
        }
        let _ =
            cleanup_programmable_resources(&client, &image.reference, &volume.name, network).await;
        return Err(error);
    }
    cleanup_result?;
    cleanup_programmable_resources(&client, &image.reference, &volume.name, network).await?;
    std::fs::remove_dir_all(&context)?;

    require(
        !sandbox.is_running().await?,
        "killed Sandbox still reports itself running",
    )?;
    require(
        !home.join("boxes").join(&sandbox_id).exists(),
        "Sandbox runtime directory remained after kill",
    )?;
    require(
        Sandbox::connect_with_client(A3sBoxClient::from_home(&home), sandbox_id)
            .await
            .is_err(),
        "Sandbox execution record remained connectable after kill",
    )?;
    Ok(())
}

async fn exercise(
    sandbox: &Sandbox,
    client: &A3sBoxClient,
    expected_isolation: ExecutionIsolation,
    image: &str,
) -> Result<(), AnyError> {
    require(
        sandbox.isolation() == expected_isolation,
        "created Sandbox reports the wrong isolation level",
    )?;
    require(sandbox.is_running().await?, "Sandbox is not running")?;

    let command = sandbox.commands.run("printf 'rust-sdk-ok'").await?;
    require(command.exit_code == 0, "Rust SDK command failed")?;
    require(
        command.stdout == "rust-sdk-ok",
        "Rust SDK command returned unexpected stdout",
    )?;
    let script = sandbox
        .script("printf 'rust-script-ok'\n")
        .env("CI", "true")
        .cwd("/workspace")
        .run()
        .await?;
    require(script.exit_code == 0, "Rust SDK script failed")?;
    require(
        script.stdout == "rust-script-ok",
        "Rust SDK script returned unexpected stdout",
    )?;
    sandbox.files.write("/cache/marker.txt", "cache-ok").await?;
    require(
        sandbox.files.read_text("/cache/marker.txt").await? == "cache-ok",
        "named volume mount did not preserve written content",
    )?;

    let directory = "/tmp/a3s-local-sdk-smoke";
    let source = format!("{directory}/source.txt");
    let destination = format!("{directory}/moved.txt");
    sandbox.files.make_dir(directory).await?;
    let write = sandbox.files.write(&source, "hello").await?;
    require(
        write.size == 5,
        "Rust SDK file write reported the wrong size",
    )?;
    require(
        sandbox.files.read_text(&source).await? == "hello",
        "Rust SDK file read returned unexpected data",
    )?;
    require(
        sandbox.files.stat(&source).await?.size == 5,
        "Rust SDK stat returned the wrong size",
    )?;
    require(
        sandbox
            .files
            .list(directory, 1)
            .await?
            .iter()
            .any(|entry| entry.path == source),
        "Rust SDK list did not return the created file",
    )?;
    sandbox.files.move_path(&source, &destination).await?;
    require(
        sandbox.files.exists(&destination).await?,
        "Rust SDK move did not publish the destination",
    )?;
    sandbox.files.remove(directory).await?;

    if expected_isolation == ExecutionIsolation::Sandbox {
        exercise_filesystem_snapshot(sandbox, client, image).await?;
    }

    sandbox.pause(true).await?;
    require(
        !sandbox.is_running().await?,
        "paused Sandbox reports itself running",
    )?;
    sandbox.resume().await?;
    require(
        sandbox.is_running().await?,
        "resumed Sandbox is not running",
    )?;
    Ok(())
}

async fn cleanup_programmable_resources(
    client: &A3sBoxClient,
    image: &str,
    volume: &str,
    network: Option<a3s_box_sdk::NetworkSummary>,
) -> Result<(), AnyError> {
    client.remove_volume(volume, false)?;
    if let Some(network) = network {
        client.remove_network(&network.name)?;
    }
    client.remove_image(image).await?;
    Ok(())
}

async fn exercise_filesystem_snapshot(
    sandbox: &Sandbox,
    client: &A3sBoxClient,
    image: &str,
) -> Result<(), AnyError> {
    // `/tmp` is an intentionally ephemeral tmpfs in Sandbox isolation and is
    // therefore outside a filesystem snapshot. Keep the marker in the rootfs.
    let marker = "/a3s-sdk-snapshot-marker.txt";
    sandbox.files.write(marker, "snapshot-ok").await?;
    let snapshot_id =
        ExecutionSnapshotId::new(format!("sdk-smoke-{}", sandbox.id().replace('-', "_")))?;
    let snapshot = sandbox
        .create_filesystem_snapshot(snapshot_id.clone())
        .await?;
    require(
        snapshot.snapshot_id == snapshot_id,
        "snapshot returned the wrong identity",
    )?;
    require(
        client.execution_snapshot_size(&snapshot_id).await? == Some(snapshot.size_bytes),
        "snapshot size lookup did not return the published size",
    )?;

    let restored = Sandbox::create_with_client(
        client.clone(),
        SandboxCreateOptions::new(image)
            .isolation(ExecutionIsolation::Sandbox)
            .filesystem_snapshot(snapshot_id.clone()),
    )
    .await?;
    let restore_result = async {
        require(
            restored.files.read_text(marker).await? == "snapshot-ok",
            "restored Sandbox did not contain the captured file",
        )?;
        require(
            client
                .delete_execution_snapshot(&snapshot_id)
                .await
                .is_err(),
            "snapshot deletion succeeded while a restored Sandbox was active",
        )?;
        Ok::<(), AnyError>(())
    }
    .await;
    let cleanup_result = restored.kill().await;
    restore_result?;
    cleanup_result?;

    require(
        client.delete_execution_snapshot(&snapshot_id).await?,
        "snapshot was not deleted after its restored Sandbox exited",
    )?;
    require(
        client
            .execution_snapshot_size(&snapshot_id)
            .await?
            .is_none(),
        "deleted snapshot still reported a size",
    )?;
    Ok(())
}

fn requested_isolation() -> Result<ExecutionIsolation, AnyError> {
    match std::env::var("A3S_BOX_SDK_SMOKE_ISOLATION")
        .unwrap_or_else(|_| "microvm".to_string())
        .as_str()
    {
        "microvm" => Ok(ExecutionIsolation::Microvm),
        "sandbox" => Ok(ExecutionIsolation::Sandbox),
        value => Err(failure(format!(
            "A3S_BOX_SDK_SMOKE_ISOLATION must be microvm or sandbox, got {value:?}"
        ))),
    }
}

fn validated_home() -> Result<PathBuf, AnyError> {
    let home = std::env::var_os("A3S_HOME")
        .map(PathBuf::from)
        .ok_or_else(|| failure("A3S_HOME must point to a dedicated local-sdk-smoke directory"))?;
    require(home.is_absolute(), "A3S_HOME must be absolute")?;
    require(
        home.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains("local-sdk-smoke")),
        "A3S_HOME must name a dedicated local-sdk-smoke directory",
    )?;
    Ok(home)
}

fn require(condition: bool, message: impl Into<String>) -> Result<(), AnyError> {
    if condition {
        Ok(())
    } else {
        Err(failure(message))
    }
}

fn failure(message: impl Into<String>) -> AnyError {
    Box::new(ClientError::Validation(message.into()))
}
