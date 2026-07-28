//! `a3s-box port-forward` command — forward host loopback TCP to a Sandbox box.

use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(any(target_os = "linux", test))]
use std::time::Duration;

#[cfg(any(target_os = "linux", test))]
use a3s_box_core::{ExecutionId, ExecutionPortConnector};
use clap::Args;
#[cfg(any(target_os = "linux", test))]
use tokio::io::{AsyncRead, AsyncWrite};

#[cfg(target_os = "linux")]
use crate::resolve;
#[cfg(target_os = "linux")]
use crate::state::StateFile;

#[derive(Args)]
pub struct PortForwardArgs {
    /// Box name or ID
    pub r#box: String,

    /// Host loopback TCP port to listen on
    #[arg(long)]
    pub host_port: NonZeroU16,

    /// TCP port exposed on the Sandbox loopback interface
    #[arg(long)]
    pub guest_port: NonZeroU16,

    /// Maximum number of concurrent forwarded connections
    #[arg(long, default_value = "64")]
    pub max_connections: NonZeroUsize,

    /// Timeout for each connection to the Sandbox workload
    #[arg(long, default_value = "5")]
    pub connect_timeout_secs: NonZeroU64,
}

#[cfg(not(target_os = "linux"))]
pub async fn execute(_args: PortForwardArgs) -> Result<(), Box<dyn std::error::Error>> {
    Err("Sandbox port forwarding requires Linux network namespaces".into())
}

#[cfg(target_os = "linux")]
pub async fn execute(args: PortForwardArgs) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write as _;

    use a3s_box_runtime::LocalExecutionManager;
    use tokio::net::TcpListener;
    use tokio::sync::Semaphore;

    let (execution_id, generation, box_name) = {
        let state = StateFile::load_default()?;
        let record = resolve::resolve(&state, &args.r#box)?;
        if record.status != "running" {
            return Err(format!("Box {} is not running", record.name).into());
        }
        let metadata = record.managed_execution.as_ref().ok_or_else(|| {
            format!(
                "Box {} is not owned by the generation-fenced execution manager",
                record.name
            )
        })?;
        if !metadata.plan.backend.is_sandbox() {
            return Err(format!(
                "Box {} does not expose a Sandbox network namespace",
                record.name
            )
            .into());
        }

        (
            ExecutionId::new(record.id.clone())?,
            metadata.generation,
            record.name.clone(),
        )
    };

    let home = a3s_box_core::dirs_home();
    let connector = Arc::new(LocalExecutionManager::with_vm_backend(
        home.join("boxes.json"),
        &home,
    ));
    let bind_address =
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, args.host_port.get()));
    let listener = TcpListener::bind(bind_address).await.map_err(|error| {
        format!("Failed to bind Box port-forward listener at {bind_address}: {error}")
    })?;
    let connection_limit = Arc::new(Semaphore::new(args.max_connections.get()));
    let connect_timeout = Duration::from_secs(args.connect_timeout_secs.get());

    println!(
        "Forwarding {bind_address} to {box_name}:{}",
        args.guest_port
    );
    std::io::stdout().flush()?;

    loop {
        let permit = Arc::clone(&connection_limit)
            .acquire_owned()
            .await
            .map_err(|_| "Box port-forward connection limiter closed")?;
        let (host_stream, peer_address) = listener
            .accept()
            .await
            .map_err(|error| format!("Failed to accept a Box port-forward connection: {error}"))?;
        host_stream.set_nodelay(true)?;

        let connector = Arc::clone(&connector);
        let execution_id = execution_id.clone();
        let guest_port = args.guest_port;
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = proxy_connection(
                connector.as_ref(),
                &execution_id,
                generation,
                guest_port,
                connect_timeout,
                host_stream,
            )
            .await
            {
                tracing::warn!(
                    peer = %peer_address,
                    execution_id = %execution_id,
                    guest_port = guest_port.get(),
                    error = %error,
                    "Sandbox port-forward connection failed"
                );
            }
        });
    }
}

#[cfg(any(target_os = "linux", test))]
async fn proxy_connection<C, S>(
    connector: &C,
    execution_id: &ExecutionId,
    generation: a3s_box_core::ExecutionGeneration,
    guest_port: NonZeroU16,
    connect_timeout: Duration,
    mut host_stream: S,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    C: ExecutionPortConnector + ?Sized,
    S: AsyncRead + AsyncWrite + Send + Unpin,
{
    let mut guest_stream = connector
        .connect_port(execution_id, generation, guest_port, connect_timeout)
        .await?;
    tokio::io::copy_bidirectional(&mut host_stream, &mut guest_stream)
        .await
        .map_err(|error| format!("Failed to relay Sandbox port-forward traffic: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use a3s_box_core::{
        ExecutionGeneration, ExecutionManagerError, ExecutionManagerResult, ExecutionPortStream,
    };
    use async_trait::async_trait;
    use clap::Parser;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    struct StubConnector {
        stream: Mutex<Option<ExecutionPortStream>>,
    }

    #[test]
    fn parses_a_bounded_loopback_forward() {
        let cli = crate::commands::Cli::try_parse_from([
            "a3s-box",
            "port-forward",
            "database",
            "--host-port",
            "54320",
            "--guest-port",
            "5432",
        ])
        .unwrap();
        let crate::commands::Command::PortForward(args) = cli.command else {
            panic!("expected port-forward command");
        };

        assert_eq!(args.r#box, "database");
        assert_eq!(args.host_port.get(), 54320);
        assert_eq!(args.guest_port.get(), 5432);
        assert_eq!(args.max_connections.get(), 64);
        assert_eq!(args.connect_timeout_secs.get(), 5);
    }

    #[test]
    fn rejects_zero_ports_and_connection_limits() {
        for invalid_args in [
            &["--host-port", "0", "--guest-port", "5432"][..],
            &["--host-port", "54320", "--guest-port", "0"][..],
            &[
                "--host-port",
                "54320",
                "--guest-port",
                "5432",
                "--max-connections",
                "0",
            ][..],
        ] {
            let mut args = vec!["a3s-box", "port-forward", "database"];
            args.extend(invalid_args);
            assert!(crate::commands::Cli::try_parse_from(args).is_err());
        }
    }

    #[async_trait]
    impl ExecutionPortConnector for StubConnector {
        async fn connect_port(
            &self,
            _execution_id: &ExecutionId,
            _generation: ExecutionGeneration,
            _port: NonZeroU16,
            _timeout: Duration,
        ) -> ExecutionManagerResult<ExecutionPortStream> {
            self.stream
                .lock()
                .expect("stub connector lock")
                .take()
                .ok_or_else(|| {
                    ExecutionManagerError::Unavailable(
                        "stub connector stream already consumed".to_string(),
                    )
                })
        }
    }

    #[tokio::test]
    async fn relays_bidirectional_bytes_through_the_canonical_connector() {
        let (host_client, host_proxy) = tokio::io::duplex(128);
        let (guest_proxy, mut guest_server) = tokio::io::duplex(128);
        let connector = StubConnector {
            stream: Mutex::new(Some(Box::pin(guest_proxy))),
        };
        let execution_id = ExecutionId::new("execution-1").unwrap();

        let proxy = tokio::spawn(async move {
            proxy_connection(
                &connector,
                &execution_id,
                ExecutionGeneration::INITIAL,
                NonZeroU16::new(5432).unwrap(),
                Duration::from_secs(1),
                host_proxy,
            )
            .await
        });
        let client = tokio::spawn(async move {
            let mut host_client = host_client;
            host_client.write_all(b"request").await.unwrap();
            let mut response = [0_u8; 8];
            host_client.read_exact(&mut response).await.unwrap();
            host_client.shutdown().await.unwrap();
            response
        });

        let mut request = [0_u8; 7];
        guest_server.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"request");
        guest_server.write_all(b"response").await.unwrap();
        guest_server.shutdown().await.unwrap();

        assert_eq!(&client.await.unwrap(), b"response");
        proxy.await.unwrap().unwrap();
    }
}
