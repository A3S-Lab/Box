use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use a3s_box_core::{
    EgressAllowlist, EgressHttpRule, EgressIpRule, EgressPolicy, EgressPolicyLimits,
    ExecutionGeneration, ExecutionId,
};
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{EgressDnsResolver, EgressProxyConfig, EgressProxyHandle};

struct StaticResolver {
    calls: AtomicUsize,
    addresses: Vec<IpAddr>,
}

#[async_trait]
impl EgressDnsResolver for StaticResolver {
    async fn resolve(&self, _hostname: &str, _port: u16) -> io::Result<Vec<IpAddr>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.addresses.clone())
    }
}

fn test_config(directory: &tempfile::TempDir, policy: EgressPolicy) -> EgressProxyConfig {
    EgressProxyConfig::new(
        ExecutionId::new("egress-proxy-test").unwrap(),
        ExecutionGeneration::INITIAL,
        policy,
        directory.path().join("generation-1.jsonl"),
    )
}

async fn read_header(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte))
            .await
            .unwrap()
            .unwrap();
        assert!(read > 0, "proxy closed before returning a response header");
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            return String::from_utf8(bytes).unwrap();
        }
    }
}

#[tokio::test]
async fn authenticated_connect_is_policy_checked_logged_and_proxied() {
    let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let target_port = target.local_addr().unwrap().port();
    let target_task = tokio::spawn(async move {
        let (mut stream, _) = target.accept().await.unwrap();
        let mut payload = [0_u8; 4];
        stream.read_exact(&mut payload).await.unwrap();
        stream.write_all(&payload).await.unwrap();
    });
    let resolver = Arc::new(StaticResolver {
        calls: AtomicUsize::new(0),
        addresses: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
    });
    let directory = tempfile::tempdir().unwrap();
    let policy = EgressPolicy::allowlist(
        [EgressHttpRule::https_on("allowed.example", target_port)],
        [EgressIpRule::tcp("127.0.0.1/32", target_port)],
    );
    let proxy =
        EgressProxyHandle::start_with_resolver(test_config(&directory, policy), resolver.clone())
            .await
            .unwrap();
    let authorization = proxy.authorization_header_value();
    let mut client = TcpStream::connect(proxy.local_address()).await.unwrap();
    client
        .write_all(
            format!(
                "CONNECT allowed.example:{target_port} HTTP/1.1\r\nHost: allowed.example:{target_port}\r\nProxy-Authorization: {authorization}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    assert!(read_header(&mut client).await.starts_with("HTTP/1.1 200"));
    client.write_all(b"ping").await.unwrap();
    let mut echoed = [0_u8; 4];
    client.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"ping");
    drop(client);
    target_task.await.unwrap();
    proxy.stop().await.unwrap();

    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    let log = tokio::fs::read_to_string(directory.path().join("generation-1.jsonl"))
        .await
        .unwrap();
    assert!(log.contains("http_and_ip_rule_matched"));
    assert!(log.contains("allowed.example"));
    assert!(!log.contains(&authorization));
    assert!(!log.contains("proxy-authorization"));
}

#[tokio::test]
async fn denied_and_unauthenticated_requests_never_resolve_or_connect() {
    let resolver = Arc::new(StaticResolver {
        calls: AtomicUsize::new(0),
        addresses: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
    });
    let directory = tempfile::tempdir().unwrap();
    let proxy = EgressProxyHandle::start_with_resolver(
        test_config(&directory, EgressPolicy::allow_domains(["allowed.example"])),
        resolver.clone(),
    )
    .await
    .unwrap();

    let mut client = TcpStream::connect(proxy.local_address()).await.unwrap();
    client
        .write_all(
            b"CONNECT allowed.example:443 HTTP/1.1\r\nHost: allowed.example:443\r\nProxy-Authorization: Bearer wrong-token\r\n\r\n",
        )
        .await
        .unwrap();
    assert!(read_header(&mut client).await.starts_with("HTTP/1.1 407"));

    let mut client = TcpStream::connect(proxy.local_address()).await.unwrap();
    let authorization = proxy.authorization_header_value();
    client
        .write_all(
            format!(
                "CONNECT denied.example:443 HTTP/1.1\r\nHost: denied.example:443\r\nProxy-Authorization: {authorization}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    assert!(read_header(&mut client).await.starts_with("HTTP/1.1 403"));
    proxy.stop().await.unwrap();

    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    let log = tokio::fs::read_to_string(directory.path().join("generation-1.jsonl"))
        .await
        .unwrap();
    assert!(log.contains("authentication_failed"));
    assert!(log.contains("hostname_not_allowed"));
    assert!(!log.contains("wrong-token"));
}

#[tokio::test]
async fn absolute_http_is_rewritten_and_proxy_shutdown_closes_connections() {
    let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let target_port = target.local_addr().unwrap().port();
    let target_task = tokio::spawn(async move {
        let (mut stream, _) = target.accept().await.unwrap();
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8(bytes).unwrap();
        assert!(request.starts_with("GET /research?q=box HTTP/1.1\r\n"));
        assert!(!request.contains("Proxy-Authorization"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    let policy = EgressPolicy::allowlist(
        [EgressHttpRule::http_on("allowed.example", target_port)],
        [EgressIpRule::tcp("127.0.0.1/32", target_port)],
    );
    let proxy = EgressProxyHandle::start_with_resolver(
        test_config(&directory, policy),
        Arc::new(StaticResolver {
            calls: AtomicUsize::new(0),
            addresses: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        }),
    )
    .await
    .unwrap();
    let mut client = TcpStream::connect(proxy.local_address()).await.unwrap();
    client
        .write_all(
            format!(
                "GET http://allowed.example:{target_port}/research?q=box HTTP/1.1\r\nHost: allowed.example:{target_port}\r\nProxy-Authorization: {}\r\n\r\n",
                proxy.authorization_header_value()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8(response).unwrap().ends_with("\r\n\r\nok"));
    target_task.await.unwrap();
    proxy.stop().await.unwrap();
}

#[tokio::test]
async fn configuration_rejects_non_loopback_listener_before_creating_log() {
    let directory = tempfile::tempdir().unwrap();
    let log_path = directory.path().join("generation-1.jsonl");
    let config = test_config(&directory, EgressPolicy::DenyAll)
        .with_bind_address(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
    assert!(EgressProxyHandle::start(config).await.is_err());
    assert!(!log_path.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn policy_listener_is_owner_only_running_and_removed_on_stop() {
    let directory = tempfile::tempdir().unwrap();
    let socket_path = directory.path().join("policy").join("generation.sock");
    let proxy = EgressProxyHandle::start(
        test_config(&directory, EgressPolicy::DenyAll).with_policy_socket_path(&socket_path),
    )
    .await
    .unwrap();

    assert!(proxy.is_running());
    let mode = std::fs::symlink_metadata(&socket_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);

    proxy.stop().await.unwrap();

    assert!(!socket_path.exists());
}

#[tokio::test]
async fn stopping_proxy_closes_active_listener_tunnel_and_upstream_socket() {
    let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let target_port = target.local_addr().unwrap().port();
    let target_task = tokio::spawn(async move {
        let (mut stream, _) = target.accept().await.unwrap();
        let mut byte = [0_u8; 1];
        tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte)).await
    });
    let directory = tempfile::tempdir().unwrap();
    let policy = EgressPolicy::allowlist(
        [EgressHttpRule::https_on("allowed.example", target_port)],
        [EgressIpRule::tcp("127.0.0.1/32", target_port)],
    );
    let proxy = EgressProxyHandle::start_with_resolver(
        test_config(&directory, policy),
        Arc::new(StaticResolver {
            calls: AtomicUsize::new(0),
            addresses: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        }),
    )
    .await
    .unwrap();
    assert!(proxy.is_running());
    let mut client = TcpStream::connect(proxy.local_address()).await.unwrap();
    client
        .write_all(
            format!(
                "CONNECT allowed.example:{target_port} HTTP/1.1\r\nHost: allowed.example:{target_port}\r\nProxy-Authorization: {}\r\n\r\n",
                proxy.authorization_header_value()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    assert!(read_header(&mut client).await.starts_with("HTTP/1.1 200"));

    proxy.stop().await.unwrap();
    let client_result = tokio::time::timeout(Duration::from_secs(2), client.read(&mut [0_u8; 1]))
        .await
        .expect("proxy shutdown must close the client tunnel");
    assert!(matches!(client_result, Ok(0) | Err(_)));
    let target_result = target_task.await.unwrap();
    assert!(matches!(target_result, Ok(Ok(0)) | Ok(Err(_))));
}

#[tokio::test]
async fn exhausted_required_decision_log_prevents_the_next_outbound_connect() {
    let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let target_port = target.local_addr().unwrap().port();
    let target_task = tokio::spawn(async move {
        let first = tokio::time::timeout(Duration::from_secs(2), target.accept())
            .await
            .unwrap()
            .unwrap();
        drop(first);
        tokio::time::timeout(Duration::from_millis(300), target.accept()).await
    });
    let directory = tempfile::tempdir().unwrap();
    let mut limits = EgressPolicyLimits::default();
    limits.max_decision_records = 2;
    let policy = EgressPolicy::Allowlist(
        EgressAllowlist::new(
            [EgressHttpRule::https_on("allowed.example", target_port)],
            [EgressIpRule::tcp("127.0.0.1/32", target_port)],
        )
        .with_limits(limits),
    );
    let proxy = EgressProxyHandle::start_with_resolver(
        test_config(&directory, policy),
        Arc::new(StaticResolver {
            calls: AtomicUsize::new(0),
            addresses: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        }),
    )
    .await
    .unwrap();
    let authorization = proxy.authorization_header_value();
    let request = format!(
        "CONNECT allowed.example:{target_port} HTTP/1.1\r\nHost: allowed.example:{target_port}\r\nProxy-Authorization: {authorization}\r\n\r\n"
    );

    let mut first = TcpStream::connect(proxy.local_address()).await.unwrap();
    first.write_all(request.as_bytes()).await.unwrap();
    assert!(read_header(&mut first).await.starts_with("HTTP/1.1 200"));
    drop(first);

    let mut second = TcpStream::connect(proxy.local_address()).await.unwrap();
    second.write_all(request.as_bytes()).await.unwrap();
    assert!(read_header(&mut second).await.starts_with("HTTP/1.1 503"));
    drop(second);
    let second_accept = target_task.await.unwrap();
    assert!(
        second_accept.is_err(),
        "a second upstream socket was created"
    );
    proxy.stop().await.unwrap();

    let log = tokio::fs::read_to_string(directory.path().join("generation-1.jsonl"))
        .await
        .unwrap();
    assert_eq!(log.lines().count(), 2);
    assert!(log.lines().last().unwrap().contains("budget_exhausted"));
}
