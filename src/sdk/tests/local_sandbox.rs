//! Opt-in real-runtime proof for the zero-configuration E2B-style Rust API.

use std::error::Error;
use std::path::PathBuf;

use a3s_box_sdk::{A3sBoxClient, ClientError, ExecutionIsolation, Sandbox, SandboxCreateOptions};

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
    let image =
        std::env::var("A3S_BOX_SDK_SMOKE_IMAGE").unwrap_or_else(|_| "alpine:3.20".to_string());
    let sandbox = Sandbox::create_with_client(
        A3sBoxClient::from_home(&home),
        SandboxCreateOptions::new(image)
            .timeout_seconds(300)
            .metadata("purpose", "local-sdk-smoke")
            .isolation(isolation),
    )
    .await?;
    let sandbox_id = sandbox.id().to_string();

    let exercise_result = exercise(&sandbox, isolation).await;
    let cleanup_result = sandbox.kill().await;
    if let Err(error) = exercise_result {
        if let Err(cleanup_error) = cleanup_result {
            eprintln!("local SDK cleanup also failed: {cleanup_error}");
        }
        return Err(error);
    }
    cleanup_result?;

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
    expected_isolation: ExecutionIsolation,
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
