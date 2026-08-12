use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::process::Command;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use a3s_box_core::{
    ExecOutput, ExecRequest, ExecutionGeneration, ExecutionId, ExecutionSessionManager,
};
use a3s_box_runtime::{egress_proxy::EgressDecisionRecord, LocalExecutionManager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

pub type TestResult<T = ()> = Result<T, String>;

const EXEC_TIMEOUT_NS: u64 = 20_000_000_000;

pub struct TcpService {
    address: SocketAddrV4,
    accepts: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl TcpService {
    pub async fn start(bind_ip: Ipv4Addr, response: impl Into<Vec<u8>>) -> TestResult<Self> {
        Self::start_inner(bind_ip, response.into(), false).await
    }

    pub async fn start_after_read(
        bind_ip: Ipv4Addr,
        response: impl Into<Vec<u8>>,
    ) -> TestResult<Self> {
        Self::start_inner(bind_ip, response.into(), true).await
    }

    async fn start_inner(
        bind_ip: Ipv4Addr,
        response: Vec<u8>,
        read_before_response: bool,
    ) -> TestResult<Self> {
        let listener = tokio::net::TcpListener::bind(SocketAddrV4::new(bind_ip, 0))
            .await
            .map_err(|error| format!("failed to bind TCP fixture on {bind_ip}: {error}"))?;
        let address = match listener
            .local_addr()
            .map_err(|error| format!("failed to inspect TCP fixture address: {error}"))?
        {
            SocketAddr::V4(address) => address,
            SocketAddr::V6(address) => {
                return Err(format!(
                    "TCP fixture unexpectedly bound an IPv6 address: {address}"
                ));
            }
        };
        let accepts = Arc::new(AtomicUsize::new(0));
        let task_accepts = Arc::clone(&accepts);
        let response = Arc::new(response);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                task_accepts.fetch_add(1, Ordering::Relaxed);
                let response = Arc::clone(&response);
                tokio::spawn(async move {
                    if read_before_response {
                        let mut request = [0_u8; 4096];
                        let Ok(Ok(read)) =
                            tokio::time::timeout(Duration::from_secs(3), stream.read(&mut request))
                                .await
                        else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                    }
                    let _ = stream.write_all(&response).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Ok(Self {
            address,
            accepts,
            task,
        })
    }

    pub const fn address(&self) -> SocketAddrV4 {
        self.address
    }

    pub fn accepts(&self) -> usize {
        self.accepts.load(Ordering::Relaxed)
    }
}

impl Drop for TcpService {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct UdpService {
    address: SocketAddrV4,
    receives: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl UdpService {
    pub async fn start(bind_ip: Ipv4Addr, port: u16) -> TestResult<Self> {
        let socket = tokio::net::UdpSocket::bind(SocketAddrV4::new(bind_ip, port))
            .await
            .map_err(|error| format!("failed to bind UDP fixture on {bind_ip}:{port}: {error}"))?;
        let address = match socket
            .local_addr()
            .map_err(|error| format!("failed to inspect UDP fixture address: {error}"))?
        {
            SocketAddr::V4(address) => address,
            SocketAddr::V6(address) => {
                return Err(format!(
                    "UDP fixture unexpectedly bound an IPv6 address: {address}"
                ));
            }
        };
        let receives = Arc::new(AtomicUsize::new(0));
        let task_receives = Arc::clone(&receives);
        let task = tokio::spawn(async move {
            let mut buffer = [0_u8; 4096];
            loop {
                let Ok((read, peer)) = socket.recv_from(&mut buffer).await else {
                    return;
                };
                task_receives.fetch_add(1, Ordering::Relaxed);
                let _ = socket.send_to(&buffer[..read], peer).await;
            }
        });
        Ok(Self {
            address,
            receives,
            task,
        })
    }

    pub const fn address(&self) -> SocketAddrV4 {
        self.address
    }

    pub fn receives(&self) -> usize {
        self.receives.load(Ordering::Relaxed)
    }
}

impl Drop for UdpService {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct HostServices {
    pub host_ip: Ipv4Addr,
    pub raw_tcp: TcpService,
    pub direct_http: TcpService,
    pub hostname_http: TcpService,
    pub hostname_connect: TcpService,
    pub udp: UdpService,
    pub dns: UdpService,
}

impl HostServices {
    pub async fn start() -> TestResult<Self> {
        let host_ip = primary_ipv4()?;
        let raw_tcp = TcpService::start_after_read(host_ip, b"raw-egress-ok\n".to_vec()).await?;
        let direct_http = TcpService::start(host_ip, http_response("direct-http-ok")).await?;
        let hostname_http =
            TcpService::start(Ipv4Addr::LOCALHOST, http_response("hostname-http-ok")).await?;
        let hostname_connect =
            TcpService::start(Ipv4Addr::LOCALHOST, b"hostname-connect-ok\n".to_vec()).await?;
        let udp = UdpService::start(host_ip, 0).await?;
        // Bind only the address targeted by the guest so the fixture can
        // coexist with a host DNS listener on another interface. macOS may
        // reject a privileged-port bind scoped to a utun address while
        // permitting the wildcard listener, so retain that fallback.
        let dns =
            match UdpService::start(host_ip, 53).await {
                Ok(service) => service,
                Err(scoped_error) => UdpService::start(Ipv4Addr::UNSPECIFIED, 53).await.map_err(
                    |wildcard_error| {
                        format!(
                            "failed to bind DNS fixture on {host_ip}:53 ({scoped_error}); \
                         wildcard fallback also failed ({wildcard_error})"
                        )
                    },
                )?,
            };
        Ok(Self {
            host_ip,
            raw_tcp,
            direct_http,
            hostname_http,
            hostname_connect,
            udp,
            dns,
        })
    }
}

pub async fn execute_script(
    manager: &LocalExecutionManager,
    execution_id: &ExecutionId,
    generation: ExecutionGeneration,
    script: impl Into<String>,
    env: Vec<String>,
) -> TestResult<ExecOutput> {
    manager
        .execute(
            execution_id,
            generation,
            ExecRequest {
                request_id: None,
                cmd: vec!["sh".to_string(), "-c".to_string(), script.into()],
                timeout_ns: EXEC_TIMEOUT_NS,
                env,
                working_dir: None,
                rootfs: None,
                stdin: None,
                stdin_streaming: false,
                user: None,
                streaming: false,
            },
        )
        .await
        .map_err(|error| format!("guest command transport failed: {error}"))
}

pub fn require_success(output: &ExecOutput, marker: &str, label: &str) -> TestResult {
    if output.exit_code != 0 {
        return Err(format!(
            "{label} exited with {}; stdout={:?}; stderr={:?}",
            output.exit_code,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    if output.truncated {
        return Err(format!("{label} output was truncated"));
    }
    if !String::from_utf8_lossy(&output.stdout).contains(marker) {
        return Err(format!(
            "{label} did not emit {marker:?}; stdout={:?}; stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

pub fn require_denied(output: &ExecOutput, forbidden_marker: &str, label: &str) -> TestResult {
    require_not_reached(output, forbidden_marker, label)?;
    if output.exit_code == 0 {
        return Err(format!(
            "{label} unexpectedly exited successfully; stdout={:?}; stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

pub fn require_not_reached(output: &ExecOutput, forbidden_marker: &str, label: &str) -> TestResult {
    if output.truncated {
        return Err(format!("{label} output was truncated"));
    }
    if String::from_utf8_lossy(&output.stdout).contains(forbidden_marker) {
        return Err(format!("{label} reached the forbidden host service"));
    }
    Ok(())
}

pub fn connect_script(host: &str, port: u16) -> String {
    format!(
        r#"set -eu
proxy="${{HTTPS_PROXY:?missing HTTPS_PROXY}}"
credentials="${{proxy#http://}}"
credentials="${{credentials%@*}}"
auth="$(printf '%s' "$credentials" | base64 | tr -d '\n')"
{{ printf 'CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\nProxy-Authorization: Basic %s\r\nConnection: close\r\n\r\n' "$auth"; sleep 1; }} |
  nc -w 4 10.90.0.1 3128
"#
    )
}

pub fn empty_no_proxy_env() -> Vec<String> {
    vec!["NO_PROXY=".to_string(), "no_proxy=".to_string()]
}

pub fn cleared_proxy_env() -> Vec<String> {
    [
        "HTTP_PROXY=",
        "http_proxy=",
        "HTTPS_PROXY=",
        "https_proxy=",
        "ALL_PROXY=",
        "all_proxy=",
        "NO_PROXY=*",
        "no_proxy=*",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

pub fn proxy_override_env(proxy_url: &str) -> Vec<String> {
    vec![
        format!("HTTP_PROXY={proxy_url}"),
        format!("http_proxy={proxy_url}"),
        format!("HTTPS_PROXY={proxy_url}"),
        format!("https_proxy={proxy_url}"),
        "NO_PROXY=".to_string(),
        "no_proxy=".to_string(),
    ]
}

pub fn proxy_credential(proxy_url: &str) -> TestResult<String> {
    let without_scheme = proxy_url
        .strip_prefix("http://")
        .ok_or_else(|| "guest proxy URL did not use http://".to_string())?;
    let (credential, _) = without_scheme
        .split_once('@')
        .ok_or_else(|| "guest proxy URL did not contain scoped credentials".to_string())?;
    if credential.is_empty() {
        return Err("guest proxy URL contained empty credentials".to_string());
    }
    Ok(credential.to_string())
}

pub fn validate_decision_log(
    path: &std::path::Path,
    execution_id: &ExecutionId,
    generation: ExecutionGeneration,
    secrets: &[String],
) -> TestResult<usize> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("failed to read decision log {}: {error}", path.display()))?;
    let limits = a3s_box_core::EgressPolicyLimits::default();
    if bytes.len() as u64 > limits.max_decision_log_bytes {
        return Err(format!(
            "decision log {} exceeded its byte bound",
            path.display()
        ));
    }
    let content = String::from_utf8(bytes)
        .map_err(|error| format!("decision log {} was not UTF-8: {error}", path.display()))?;
    for needle in ["proxy-authorization", "http://a3s:"] {
        if content.to_ascii_lowercase().contains(needle) {
            return Err(format!(
                "decision log {} leaked proxy material",
                path.display()
            ));
        }
    }
    for secret in secrets {
        if !secret.is_empty() && content.contains(secret) {
            return Err(format!(
                "decision log {} leaked a generation credential",
                path.display()
            ));
        }
    }

    let mut previous_sequence = 0;
    let mut records = 0_usize;
    for line in content.lines() {
        let record: EgressDecisionRecord = serde_json::from_str(line).map_err(|error| {
            format!(
                "decision log {} contained invalid JSON: {error}",
                path.display()
            )
        })?;
        if &record.execution_id != execution_id || record.generation != generation {
            return Err(format!(
                "decision log {} crossed execution or generation scope",
                path.display()
            ));
        }
        if record.sequence <= previous_sequence {
            return Err(format!(
                "decision log {} sequence was not monotonic",
                path.display()
            ));
        }
        previous_sequence = record.sequence;
        records += 1;
    }
    if records == 0 {
        return Err(format!("decision log {} was empty", path.display()));
    }
    if records > limits.max_decision_records as usize {
        return Err(format!(
            "decision log {} exceeded its record bound",
            path.display()
        ));
    }
    Ok(records)
}

pub fn decision_log_path(
    home: &std::path::Path,
    execution_id: &ExecutionId,
    generation: ExecutionGeneration,
) -> std::path::PathBuf {
    home.join("boxes")
        .join(execution_id.as_str())
        .join("security")
        .join("egress")
        .join(format!("generation-{}", generation.get()))
        .join("decisions.jsonl")
}

pub fn policy_socket_path(
    execution_id: &ExecutionId,
    generation: ExecutionGeneration,
) -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    let root = std::path::Path::new("/private/tmp/a3s-box-sockets");
    #[cfg(not(target_os = "macos"))]
    let root = std::path::Path::new("/tmp/a3s-box-sockets");
    root.join(execution_id.as_str())
        .join(format!("egress-generation-{}.sock", generation.get()))
}

pub fn shim_count() -> usize {
    let Ok(output) = Command::new("pgrep").args(["-x", "a3s-box-shim"]).output() else {
        return 0;
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

pub async fn wait_for_counter(counter: impl Fn() -> usize, expected: usize) -> TestResult {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let actual = counter();
        if actual == expected {
            return Ok(());
        }
        if actual > expected {
            return Err(format!(
                "host fixture received {actual} connections or datagrams; expected {expected}"
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "host fixture received {actual} connections or datagrams; expected {expected}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

fn primary_ipv4() -> TestResult<Ipv4Addr> {
    let mut interfaces = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut interfaces) } != 0 {
        return Err(format!(
            "failed to enumerate host interfaces: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut candidates = Vec::new();
    let mut current = interfaces;
    while !current.is_null() {
        let interface = unsafe { &*current };
        let address = interface.ifa_addr;
        if !address.is_null()
            && unsafe { (*address).sa_family as i32 } == libc::AF_INET
            && interface.ifa_flags & libc::IFF_UP as u32 != 0
            && interface.ifa_flags & libc::IFF_LOOPBACK as u32 == 0
        {
            let socket_address = unsafe { &*(address.cast::<libc::sockaddr_in>()) };
            let candidate = Ipv4Addr::from(u32::from_be(socket_address.sin_addr.s_addr));
            if !candidate.is_unspecified()
                && !candidate.is_link_local()
                && !candidate.is_multicast()
                && !candidates.contains(&candidate)
            {
                candidates.push(candidate);
            }
        }
        current = interface.ifa_next;
    }
    unsafe {
        libc::freeifaddrs(interfaces);
    }

    candidates.sort_by_key(|address| !address.is_private());
    for candidate in &candidates {
        let Ok(listener) = std::net::TcpListener::bind(SocketAddrV4::new(*candidate, 0)) else {
            continue;
        };
        let Ok(address) = listener.local_addr() else {
            continue;
        };
        if std::net::TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok() {
            return Ok(*candidate);
        }
    }
    Err(format!(
        "no self-connectable non-loopback IPv4 address was available; candidates={candidates:?}"
    ))
}

fn http_response(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}
