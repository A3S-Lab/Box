//! Ignored real-host qualification for restricted MicroVM egress.
//!
//! This test boots real libkrun guests and must run only on an HVF/KVM host.
//! It uses host-local fixtures so raw TCP, HTTP proxy, DNS, UDP, and cleanup
//! results do not depend on a third-party service.

#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use a3s_box_core::config::{PoolConfig, SidecarConfig};
use a3s_box_core::{
    BoxConfig, CreateExecutionRequest, EgressHttpRule, EgressIpRule, EgressPolicy,
    ExecutionGeneration, ExecutionId, ExecutionIsolation, ExecutionLease, ExecutionManager,
    ExecutionRecordPolicy, ExecutionState, NetworkMode, OperationId, ResourceConfig,
    SandboxSecurityPolicy,
};
use a3s_box_runtime::LocalExecutionManager;

mod egress_support;
mod support;

use egress_support::*;
use support::{host_smoke_image, host_socket_dirs, seed_runnable_alpine_image, CliTest};

struct TrackedExecution {
    execution_id: ExecutionId,
    generation: ExecutionGeneration,
    stopped: bool,
}

struct Harness {
    manager: LocalExecutionManager,
    home: PathBuf,
    image: String,
    executions: Vec<TrackedExecution>,
}

impl Harness {
    fn new(home: PathBuf, image: String) -> Self {
        Self {
            manager: LocalExecutionManager::with_vm_backend(home.join("boxes.json"), &home),
            home,
            image,
            executions: Vec::new(),
        }
    }

    async fn start(
        &mut self,
        name: &str,
        egress: EgressPolicy,
        dns: Vec<String>,
    ) -> TestResult<usize> {
        let operation_id = OperationId::new(format!(
            "real-egress-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ))
        .map_err(|error| error.to_string())?;
        let lease = self
            .manager
            .create_and_start(request(&self.image, name, egress, dns), &operation_id)
            .await
            .map_err(|error| format!("failed to start {name} restricted MicroVM: {error}"))?;
        self.executions.push(TrackedExecution {
            execution_id: lease.execution_id,
            generation: lease.generation,
            stopped: false,
        });
        Ok(self.executions.len() - 1)
    }

    fn identity(&self, index: usize) -> (&ExecutionId, ExecutionGeneration) {
        let tracked = &self.executions[index];
        (&tracked.execution_id, tracked.generation)
    }

    async fn script(
        &self,
        index: usize,
        script: impl Into<String>,
        env: Vec<String>,
    ) -> TestResult<a3s_box_core::ExecOutput> {
        let (execution_id, generation) = self.identity(index);
        execute_script(&self.manager, execution_id, generation, script, env).await
    }

    async fn proxy_url(&self, index: usize) -> TestResult<String> {
        let output = self
            .script(index, "printf '%s' \"${HTTP_PROXY:-}\"", Vec::new())
            .await?;
        if output.exit_code != 0 {
            return Err(format!(
                "failed to read guest proxy environment: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let value = String::from_utf8(output.stdout)
            .map_err(|error| format!("guest proxy environment was not UTF-8: {error}"))?;
        let value = value.trim().to_string();
        proxy_credential(&value)?;
        Ok(value)
    }

    async fn restart(&mut self, index: usize) -> TestResult<ExecutionLease> {
        let tracked = &self.executions[index];
        let operation_id = OperationId::new(format!(
            "real-egress-restart-{}",
            uuid::Uuid::new_v4().simple()
        ))
        .map_err(|error| error.to_string())?;
        let lease = self
            .manager
            .restart(&tracked.execution_id, tracked.generation, &operation_id)
            .await
            .map_err(|error| format!("failed to restart restricted MicroVM: {error}"))?;
        self.executions[index].generation = lease.generation;
        Ok(lease)
    }

    async fn stop(&mut self, index: usize) -> TestResult {
        if self.executions[index].stopped {
            return Ok(());
        }
        let tracked = &self.executions[index];
        self.manager
            .kill(&tracked.execution_id, tracked.generation)
            .await
            .map_err(|error| format!("failed to stop {}: {error}", tracked.execution_id))?;
        self.executions[index].stopped = true;
        Ok(())
    }

    async fn stop_all(&mut self) -> TestResult {
        let mut failures = Vec::new();
        for index in 0..self.executions.len() {
            if let Err(error) = self.stop(index).await {
                failures.push(error);
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    async fn cleanup(&mut self) -> TestResult {
        let mut failures = Vec::new();
        if let Err(error) = self.stop_all().await {
            failures.push(error);
        }
        for tracked in &mut self.executions {
            match self
                .manager
                .remove(&tracked.execution_id, tracked.generation)
                .await
            {
                Ok(true) => {}
                Ok(false) => failures.push(format!(
                    "execution {} was not removed",
                    tracked.execution_id
                )),
                Err(error) => failures.push(format!(
                    "failed to remove {}: {error}",
                    tracked.execution_id
                )),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    async fn verify_unsupported_preflight(&self) -> TestResult {
        let baseline_state = std::fs::read(self.manager.state_path()).ok();
        let baseline_boxes = directory_names(&self.home.join("boxes"))?;
        let mut cases = vec![
            (
                "sandbox",
                BoxConfig {
                    isolation: ExecutionIsolation::Sandbox,
                    ..BoxConfig::default()
                },
            ),
            (
                "bridge",
                BoxConfig {
                    network: NetworkMode::Bridge {
                        network: "unavailable".to_string(),
                    },
                    ..BoxConfig::default()
                },
            ),
            (
                "none",
                BoxConfig {
                    network: NetworkMode::None,
                    ..BoxConfig::default()
                },
            ),
            (
                "udp",
                BoxConfig {
                    security_policy: Some(SandboxSecurityPolicy::new().egress(
                        EgressPolicy::allowlist([], [EgressIpRule::udp("192.0.2.1/32", 53)]),
                    )),
                    ..BoxConfig::default()
                },
            ),
            (
                "ipv6",
                BoxConfig {
                    security_policy: Some(SandboxSecurityPolicy::new().egress(
                        EgressPolicy::allowlist([], [EgressIpRule::tcp("2001:db8::/32", 443)]),
                    )),
                    ..BoxConfig::default()
                },
            ),
            (
                "published-port",
                BoxConfig {
                    port_map: vec!["18080:80".to_string()],
                    ..BoxConfig::default()
                },
            ),
            (
                "sidecar",
                BoxConfig {
                    sidecar: Some(SidecarConfig::default()),
                    ..BoxConfig::default()
                },
            ),
            (
                "warm-pool",
                BoxConfig {
                    pool: PoolConfig {
                        enabled: true,
                        ..PoolConfig::default()
                    },
                    ..BoxConfig::default()
                },
            ),
            (
                "snapshot-fork",
                BoxConfig {
                    snapshot_mem_file: Some("memory".to_string()),
                    snapshot_sock: Some("snapshot.sock".to_string()),
                    ..BoxConfig::default()
                },
            ),
        ];
        for (name, config) in &mut cases {
            config.image = self.image.clone();
            if config.security_policy.is_none() {
                config.security_policy =
                    Some(SandboxSecurityPolicy::new().egress(EgressPolicy::DenyAll));
            }
            let operation = OperationId::new(format!(
                "real-egress-invalid-{name}-{}",
                uuid::Uuid::new_v4().simple()
            ))
            .map_err(|error| error.to_string())?;
            let result = self
                .manager
                .create(
                    CreateExecutionRequest {
                        external_sandbox_id: format!("invalid-{name}"),
                        config: config.clone(),
                        labels: BTreeMap::new(),
                        policy: ExecutionRecordPolicy::default(),
                        rootfs_snapshot_id: None,
                    },
                    &operation,
                )
                .await;
            if result.is_ok() {
                return Err(format!(
                    "unsupported restricted-egress case {name} mutated lifecycle state"
                ));
            }
            if std::fs::read(self.manager.state_path()).ok() != baseline_state
                || directory_names(&self.home.join("boxes"))? != baseline_boxes
            {
                return Err(format!(
                    "unsupported restricted-egress case {name} left host state"
                ));
            }
        }
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn test_real_restricted_egress_matrix() {
    let cli = CliTest::new();
    let image = host_smoke_image();
    seed_runnable_alpine_image(&cli, &image);

    let socket_dirs_before = host_socket_dirs();
    let shims_before = shim_count();
    let mut harness = Harness::new(cli.home_path().to_path_buf(), image);
    let services = match HostServices::start().await {
        Ok(services) => services,
        Err(error) => panic!("real restricted-egress fixture preflight failed: {error}"),
    };

    let matrix_result = run_matrix(&mut harness, &services).await;
    let cleanup_result = harness.cleanup().await;
    drop(services);
    let baseline_result =
        wait_for_cleanup_baseline(shims_before, &socket_dirs_before, cli.home_path()).await;

    if matrix_result.is_ok() && cleanup_result.is_ok() && baseline_result.is_ok() {
        record_case("cleanup");
        return;
    }

    let mut failures = Vec::new();
    if let Err(error) = matrix_result {
        failures.push(format!("matrix: {error}"));
    }
    if let Err(error) = cleanup_result {
        failures.push(format!("cleanup: {error}"));
    }
    if let Err(error) = baseline_result {
        failures.push(format!("baseline: {error}"));
    }
    panic!(
        "real restricted-egress qualification failed after cleanup: {}",
        failures.join("; ")
    );
}

async fn run_matrix(harness: &mut Harness, services: &HostServices) -> TestResult {
    harness.verify_unsupported_preflight().await?;
    record_case("unsupported_preflight");

    let deny = harness
        .start(
            "deny-all",
            EgressPolicy::DenyAll,
            vec![services.host_ip.to_string()],
        )
        .await?;
    let boot = harness
        .script(
            deny,
            "set -eu; test \"$(uname -s)\" = Linux; command -v wget; command -v nc; command -v nslookup; command -v base64; command -v timeout; printf dedicated-kernel-ok",
            Vec::new(),
        )
        .await?;
    require_success(&boot, "dedicated-kernel-ok", "dedicated-kernel guest boot")?;
    let deny_proxy = harness.proxy_url(deny).await?;
    record_case("dedicated_kernel_boot");

    let deny_http = harness
        .script(
            deny,
            format!(
                "wget -T 4 -qO- http://localhost:{}/deny",
                services.hostname_http.address().port()
            ),
            empty_no_proxy_env(),
        )
        .await?;
    require_denied(&deny_http, "hostname-http-ok", "DenyAll HTTP")?;
    let deny_https = harness
        .script(
            deny,
            connect_script("localhost", services.hostname_connect.address().port()),
            Vec::new(),
        )
        .await?;
    if !String::from_utf8_lossy(&deny_https.stdout).contains("403") {
        let network = harness
            .script(
                deny,
                "ip address; ip route; cat /proc/net/route",
                Vec::new(),
            )
            .await?;
        let (deny_id, deny_generation) = harness.identity(deny);
        let decisions =
            std::fs::read_to_string(decision_log_path(&harness.home, deny_id, deny_generation))
                .unwrap_or_else(|error| format!("<decision log unavailable: {error}>"));
        let box_dir = harness.home.join("boxes").join(deny_id.as_str());
        let shim_stdout = read_diagnostic(&box_dir.join("logs").join("shim.stdout.log"));
        let shim_stderr = read_diagnostic(&box_dir.join("logs").join("shim.stderr.log"));
        let net_stats = read_diagnostic(&box_dir.join("sockets").join("net.stats.json"));
        return Err(format!(
            "DenyAll HTTPS CONNECT produced no proxy response: exit={}; stdout={:?}; stderr={:?}; network={:?}; decisions={decisions:?}; net_stats={net_stats:?}; shim_stdout={shim_stdout:?}; shim_stderr={shim_stderr:?}",
            deny_https.exit_code,
            String::from_utf8_lossy(&deny_https.stdout),
            String::from_utf8_lossy(&deny_https.stderr),
            String::from_utf8_lossy(&network.stdout)
        ));
    }
    let deny_raw_before = services.raw_tcp.accepts();
    let deny_raw = harness
        .script(
            deny,
            format!(
                "printf denied | nc -w 4 {} {}",
                services.host_ip,
                services.raw_tcp.address().port()
            ),
            Vec::new(),
        )
        .await?;
    require_not_reached(&deny_raw, "raw-egress-ok", "DenyAll raw TCP")?;
    settle().await;
    require_counter_unchanged(
        services.raw_tcp.accepts(),
        deny_raw_before,
        "DenyAll raw TCP",
    )?;
    record_case("deny_all");

    let hostname_policy = EgressPolicy::allowlist(
        [
            EgressHttpRule::http_on("localhost", services.hostname_http.address().port()),
            EgressHttpRule::https_on("localhost", services.hostname_connect.address().port()),
        ],
        [
            EgressIpRule::tcp("127.0.0.1/32", services.hostname_http.address().port()),
            EgressIpRule::tcp("127.0.0.1/32", services.hostname_connect.address().port()),
        ],
    );
    let hostname = harness
        .start(
            "hostname-only",
            hostname_policy,
            vec![services.host_ip.to_string()],
        )
        .await?;
    let hostname_http = harness
        .script(
            hostname,
            format!(
                "wget -T 4 -qO- http://localhost:{}/allowed",
                services.hostname_http.address().port()
            ),
            empty_no_proxy_env(),
        )
        .await?;
    require_success(&hostname_http, "hostname-http-ok", "allowed hostname HTTP")?;
    let hostname_https = harness
        .script(
            hostname,
            connect_script("localhost", services.hostname_connect.address().port()),
            Vec::new(),
        )
        .await?;
    require_success(
        &hostname_https,
        "hostname-connect-ok",
        "allowed hostname HTTPS CONNECT",
    )?;
    let hostname_proxy_generation_one = harness.proxy_url(hostname).await?;
    record_case("hostname_allow");

    let unmatched = harness
        .script(
            hostname,
            "wget -T 4 -qO- http://unmatched.invalid:18080/denied",
            empty_no_proxy_env(),
        )
        .await?;
    require_denied(&unmatched, "hostname-http-ok", "unmatched hostname")?;
    record_case("hostname_deny");

    let direct_http_before = services.direct_http.accepts();
    let bypass = harness
        .script(
            hostname,
            format!(
                "wget -T 4 -qO- http://{}:{}/bypass",
                services.host_ip,
                services.direct_http.address().port()
            ),
            cleared_proxy_env(),
        )
        .await?;
    require_denied(&bypass, "direct-http-ok", "cleared proxy variables")?;
    settle().await;
    require_counter_unchanged(
        services.direct_http.accepts(),
        direct_http_before,
        "cleared proxy variables",
    )?;
    record_case("proxy_bypass");

    let raw_before = services.raw_tcp.accepts();
    let direct_ip = harness
        .script(
            hostname,
            format!(
                "printf direct | nc -w 4 {} {}",
                services.host_ip,
                services.raw_tcp.address().port()
            ),
            cleared_proxy_env(),
        )
        .await?;
    require_not_reached(&direct_ip, "raw-egress-ok", "hostname-only direct IPv4")?;
    settle().await;
    require_counter_unchanged(
        services.raw_tcp.accepts(),
        raw_before,
        "hostname-only direct IPv4",
    )?;
    record_case("direct_ipv4");

    let dns_before = services.dns.receives();
    let dns = harness
        .script(
            hostname,
            format!(
                "timeout 4 nslookup egress-policy.invalid {}",
                services.host_ip
            ),
            cleared_proxy_env(),
        )
        .await?;
    require_nonzero(&dns, "custom DNS")?;
    settle().await;
    require_counter_unchanged(services.dns.receives(), dns_before, "custom DNS")?;

    let udp_before = services.udp.receives();
    let udp = harness
        .script(
            hostname,
            format!(
                "timeout 4 sh -c 'printf quic | nc -u -w 2 {} {}'",
                services.host_ip,
                services.udp.address().port()
            ),
            cleared_proxy_env(),
        )
        .await?;
    require_no_usage_error(&udp, "UDP/QUIC attempt")?;
    settle().await;
    require_counter_unchanged(services.udp.receives(), udp_before, "UDP/QUIC")?;

    let ipv6 = harness
        .script(
            hostname,
            "nc -w 2 2001:db8::1 443 </dev/null",
            cleared_proxy_env(),
        )
        .await?;
    require_nonzero(&ipv6, "IPv6 bypass")?;
    require_no_usage_error(&ipv6, "IPv6 bypass")?;

    let doh = harness
        .script(
            hostname,
            connect_script("cloudflare-dns.com", 443),
            Vec::new(),
        )
        .await?;
    require_stdout_contains(&doh, "403", "DoH CONNECT bypass")?;
    record_case("dns_udp_ipv6_doh_quic");

    let raw_policy = EgressPolicy::allowlist(
        [],
        [EgressIpRule::tcp(
            format!("{}/32", services.host_ip),
            services.raw_tcp.address().port(),
        )],
    );
    let raw = harness
        .start("raw-ip", raw_policy, vec![services.host_ip.to_string()])
        .await?;
    let raw_proxy = harness.proxy_url(raw).await?;
    let raw_allowed = harness
        .script(
            raw,
            format!(
                "{{ printf allowed; sleep 1; }} | nc -w 4 {} {}",
                services.host_ip,
                services.raw_tcp.address().port()
            ),
            cleared_proxy_env(),
        )
        .await?;
    if let Err(error) = require_success(&raw_allowed, "raw-egress-ok", "allowed raw IPv4 TCP") {
        let (raw_id, raw_generation) = harness.identity(raw);
        let box_dir = harness.home.join("boxes").join(raw_id.as_str());
        return Err(format!(
            "{error}; host_accepts={}; decisions={:?}; net_stats={:?}",
            services.raw_tcp.accepts(),
            read_diagnostic(&decision_log_path(&harness.home, raw_id, raw_generation)),
            read_diagnostic(&box_dir.join("sockets").join("net.stats.json"))
        ));
    }
    let direct_http_before = services.direct_http.accepts();
    let raw_unmatched = harness
        .script(
            raw,
            format!(
                "printf denied | nc -w 4 {} {}",
                services.host_ip,
                services.direct_http.address().port()
            ),
            cleared_proxy_env(),
        )
        .await?;
    require_not_reached(&raw_unmatched, "direct-http-ok", "unmatched raw IPv4 port")?;
    settle().await;
    require_counter_unchanged(
        services.direct_http.accepts(),
        direct_http_before,
        "unmatched raw IPv4 port",
    )?;
    record_case("raw_ipv4");

    let concurrent_before = services.raw_tcp.accepts();
    let allowed_future = harness.script(
        raw,
        format!(
            "{{ printf allowed; sleep 1; }} | nc -w 4 {} {}",
            services.host_ip,
            services.raw_tcp.address().port()
        ),
        cleared_proxy_env(),
    );
    let denied_future = harness.script(
        deny,
        format!(
            "printf denied | nc -w 4 {} {}",
            services.host_ip,
            services.raw_tcp.address().port()
        ),
        cleared_proxy_env(),
    );
    let (allowed, denied) = tokio::join!(allowed_future, denied_future);
    require_success(&allowed?, "raw-egress-ok", "concurrent allowed execution")?;
    require_not_reached(&denied?, "raw-egress-ok", "concurrent denied execution")?;
    wait_for_counter(
        || services.raw_tcp.accepts(),
        concurrent_before.saturating_add(1),
    )
    .await?;
    record_case("concurrent_isolation");

    let (hostname_id, generation_one) = {
        let (execution_id, generation) = harness.identity(hostname);
        (execution_id.clone(), generation)
    };
    let generation_one_socket = policy_socket_path(&hostname_id, generation_one);
    if !generation_one_socket.exists() {
        return Err("hostname generation-one policy socket was missing".to_string());
    }
    let restarted = harness.restart(hostname).await?;
    if restarted.generation.get() != generation_one.get().saturating_add(1) {
        return Err("restart did not advance the egress generation exactly once".to_string());
    }
    let hostname_proxy_generation_two = harness.proxy_url(hostname).await?;
    if hostname_proxy_generation_one == hostname_proxy_generation_two {
        return Err("restart reused the old egress proxy credential".to_string());
    }
    let generation_two_socket = policy_socket_path(&hostname_id, restarted.generation);
    if generation_one_socket.exists() || !generation_two_socket.exists() {
        return Err("restart did not replace the generation-scoped policy socket".to_string());
    }
    let post_restart = harness
        .script(
            hostname,
            format!(
                "wget -T 4 -qO- http://localhost:{}/restart",
                services.hostname_http.address().port()
            ),
            empty_no_proxy_env(),
        )
        .await?;
    require_success(
        &post_restart,
        "hostname-http-ok",
        "post-restart hostname HTTP",
    )?;
    let stale_token = harness
        .script(
            hostname,
            format!(
                "wget -T 4 -qO- http://localhost:{}/stale",
                services.hostname_http.address().port()
            ),
            proxy_override_env(&hostname_proxy_generation_one),
        )
        .await?;
    require_nonzero(&stale_token, "stale generation proxy credential")?;
    settle().await;
    let generation_two_log = decision_log_path(&harness.home, &hostname_id, restarted.generation);
    let generation_two_content = std::fs::read_to_string(&generation_two_log).map_err(|error| {
        format!(
            "failed to read generation-two decision log {}: {error}",
            generation_two_log.display()
        )
    })?;
    if !generation_two_content.contains("authentication_failed") {
        return Err("stale generation credential rejection was not observable".to_string());
    }
    record_case("generation_restart");

    let (raw_id, raw_generation) = {
        let (execution_id, generation) = harness.identity(raw);
        (execution_id.clone(), generation)
    };
    let raw_credential = proxy_credential(&raw_proxy)?;
    validate_decision_log(
        &decision_log_path(&harness.home, &raw_id, raw_generation),
        &raw_id,
        raw_generation,
        std::slice::from_ref(&raw_credential),
    )?;
    let raw_policy_socket = policy_socket_path(&raw_id, raw_generation);
    std::fs::remove_file(&raw_policy_socket).map_err(|error| {
        format!(
            "failed to inject policy-channel failure at {}: {error}",
            raw_policy_socket.display()
        )
    })?;
    let raw_before_failure = services.raw_tcp.accepts();
    let channel_failure = harness
        .script(
            raw,
            format!(
                "printf denied | nc -w 4 {} {}",
                services.host_ip,
                services.raw_tcp.address().port()
            ),
            cleared_proxy_env(),
        )
        .await;
    match channel_failure {
        Ok(output) => {
            require_not_reached(&output, "raw-egress-ok", "missing policy channel")?;
        }
        Err(error) if error.contains("restricted MicroVM egress proxy is unavailable") => {}
        Err(error) => {
            return Err(format!(
                "missing policy channel produced an unexpected session error: {error}"
            ));
        }
    }
    settle().await;
    require_counter_unchanged(
        services.raw_tcp.accepts(),
        raw_before_failure,
        "missing policy channel",
    )?;
    let status = harness
        .manager
        .inspect(&raw_id)
        .await
        .map_err(|error| format!("failed to inspect policy-channel failure: {error}"))?;
    if status.state != ExecutionState::Stopped {
        return Err(format!(
            "policy-channel failure remained invisible in lifecycle state: {:?}",
            status.state
        ));
    }
    harness.executions[raw].stopped = true;
    record_case("policy_channel_failure");

    let deny_credential = proxy_credential(&deny_proxy)?;
    let hostname_credential_one = proxy_credential(&hostname_proxy_generation_one)?;
    let hostname_credential_two = proxy_credential(&hostname_proxy_generation_two)?;
    let secrets = [
        deny_credential,
        raw_credential,
        hostname_credential_one,
        hostname_credential_two,
    ];
    let (deny_id, deny_generation) = harness.identity(deny);
    validate_decision_log(
        &decision_log_path(&harness.home, deny_id, deny_generation),
        deny_id,
        deny_generation,
        &secrets,
    )?;
    validate_decision_log(
        &decision_log_path(&harness.home, &hostname_id, generation_one),
        &hostname_id,
        generation_one,
        &secrets,
    )?;
    validate_decision_log(
        &decision_log_path(&harness.home, &hostname_id, restarted.generation),
        &hostname_id,
        restarted.generation,
        &secrets,
    )?;
    record_case("decision_logs");
    harness.stop_all().await?;
    Ok(())
}

fn read_diagnostic(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| format!("<{} unavailable: {error}>", path.display()))
}

fn request(
    image: &str,
    name: &str,
    egress: EgressPolicy,
    dns: Vec<String>,
) -> CreateExecutionRequest {
    CreateExecutionRequest {
        external_sandbox_id: format!("real-egress-{name}"),
        config: BoxConfig {
            image: image.to_string(),
            resources: ResourceConfig {
                vcpus: 1,
                memory_mb: 256,
                disk_mb: 512,
                timeout: 600,
            },
            cmd: vec!["sleep".to_string(), "3600".to_string()],
            dns,
            network: NetworkMode::Tsi,
            security_policy: Some(SandboxSecurityPolicy::new().egress(egress)),
            ..BoxConfig::default()
        },
        labels: BTreeMap::from([("purpose".to_string(), "real-egress-soak".to_string())]),
        policy: ExecutionRecordPolicy {
            name: Some(format!("real-egress-{name}")),
            ..ExecutionRecordPolicy::default()
        },
        rootfs_snapshot_id: None,
    }
}

fn require_stdout_contains(
    output: &a3s_box_core::ExecOutput,
    marker: &str,
    label: &str,
) -> TestResult {
    if !String::from_utf8_lossy(&output.stdout).contains(marker) {
        return Err(format!(
            "{label} did not emit {marker:?}; exit={}; stdout={:?}; stderr={:?}",
            output.exit_code,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

fn require_nonzero(output: &a3s_box_core::ExecOutput, label: &str) -> TestResult {
    if output.exit_code == 0 {
        return Err(format!(
            "{label} unexpectedly exited successfully; stdout={:?}; stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

fn require_no_usage_error(output: &a3s_box_core::ExecOutput, label: &str) -> TestResult {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    if stderr.contains("usage:") || stderr.contains("invalid option") {
        return Err(format!(
            "{label} was not exercised because the guest command was unsupported: {stderr:?}"
        ));
    }
    Ok(())
}

fn require_counter_unchanged(actual: usize, before: usize, label: &str) -> TestResult {
    if actual != before {
        return Err(format!(
            "{label} reached a host fixture: before={before} after={actual}"
        ));
    }
    Ok(())
}

fn directory_names(path: &Path) -> TestResult<BTreeSet<String>> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Ok(BTreeSet::new());
    };
    entries
        .map(|entry| {
            entry
                .map_err(|error| format!("failed to inspect {}: {error}", path.display()))
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
        })
        .collect()
}

async fn wait_for_cleanup_baseline(
    shims_before: usize,
    socket_dirs_before: &BTreeSet<PathBuf>,
    home: &Path,
) -> TestResult {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let shims_after = shim_count();
        let socket_dirs_after = host_socket_dirs();
        let new_socket_dirs: Vec<_> = socket_dirs_after
            .difference(socket_dirs_before)
            .cloned()
            .collect();
        let box_dirs = directory_names(&home.join("boxes"))?;
        if shims_after <= shims_before && new_socket_dirs.is_empty() && box_dirs.is_empty() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "resources did not return to baseline: shims={shims_before}->{shims_after}, new_socket_dirs={new_socket_dirs:?}, box_dirs={box_dirs:?}"
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn record_case(name: &str) {
    eprintln!("A3S_EGRESS_CASE case={name} result=pass");
}
