//! Runtime health probes over Box's generation-fenced execution ports and exec
//! session boundary.

use std::num::NonZeroU16;
use std::time::Duration;

use a3s_box_core::{
    ExecRequest, ExecutionManagerError, ExecutionPortStream, ExecutionSessionManager,
};
use a3s_runtime::contract::{
    HealthProbe, RuntimeHealthCheck, RuntimeHealthObservation, RuntimeHealthState,
    RuntimeObservation, RuntimeUnitSpec, TransportProtocol,
};
use a3s_runtime::{RuntimeError, RuntimeResult};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{BoxRecord, ManagedExecutionState};

use super::metadata::{local_identity, map_execution_error, now_ms, timestamp_ms};
use super::BoxRuntimeDriver;

const NANOS_PER_MILLISECOND: u64 = 1_000_000;
const MAX_HTTP_RESPONSE_HEAD_BYTES: usize = 8 * 1024;

struct ProbeTracker<'a> {
    policy: &'a RuntimeHealthCheck,
    next_probe_at: tokio::time::Instant,
    successes: u32,
    failures: u32,
    settled: Option<RuntimeHealthObservation>,
}

impl<'a> ProbeTracker<'a> {
    fn new(policy: &'a RuntimeHealthCheck, record: &BoxRecord) -> RuntimeResult<Self> {
        Ok(Self {
            policy,
            next_probe_at: tokio::time::Instant::now()
                + remaining_start_period(record, policy.start_period_ms)?,
            successes: 0,
            failures: 0,
            settled: None,
        })
    }

    fn is_settled(&self) -> bool {
        self.settled.is_some()
    }

    fn is_due(&self, now: tokio::time::Instant) -> bool {
        !self.is_settled() && now >= self.next_probe_at
    }

    fn record(&mut self, observation: RuntimeHealthObservation) {
        match observation.state {
            RuntimeHealthState::Healthy => {
                self.successes = self.successes.saturating_add(1);
                self.failures = 0;
            }
            RuntimeHealthState::Unhealthy => {
                self.failures = self.failures.saturating_add(1);
                self.successes = 0;
            }
            RuntimeHealthState::Unknown | RuntimeHealthState::Starting => {
                self.successes = 0;
                self.failures = 0;
            }
        }
        if self.successes >= self.policy.success_threshold
            || self.failures >= self.policy.failure_threshold
        {
            self.settled = Some(observation);
        } else {
            self.next_probe_at =
                tokio::time::Instant::now() + Duration::from_millis(self.policy.interval_ms);
        }
    }
}

impl BoxRuntimeDriver {
    /// Applies the independent readiness and liveness threshold policies. The
    /// state is derived from live, generation-fenced probes and is not copied
    /// into a second health registry or lifecycle store. The Runtime operation
    /// deadline bounds complete convergence; each Box control operation remains
    /// bounded independently by the driver configuration.
    pub(super) async fn wait_for_service_health(
        &self,
        spec: &RuntimeUnitSpec,
        record: BoxRecord,
    ) -> RuntimeResult<RuntimeObservation> {
        self.settle_service_health(spec, record).await
    }

    /// Produces threshold-settled readiness and liveness evidence for inspect
    /// and exec observations without creating a parallel durable registry.
    pub(super) async fn observe_service_health(
        &self,
        spec: &RuntimeUnitSpec,
        record: &BoxRecord,
    ) -> RuntimeResult<RuntimeObservation> {
        self.settle_service_health(spec, record.clone()).await
    }

    async fn settle_service_health(
        &self,
        spec: &RuntimeUnitSpec,
        mut record: BoxRecord,
    ) -> RuntimeResult<RuntimeObservation> {
        if spec.health.is_none() && spec.service_lifecycle.is_none() {
            return self.observation(spec, &record, None, None).await;
        }
        if local_identity(&record)?.2 != ManagedExecutionState::Running {
            return self.observation(spec, &record, None, None).await;
        }

        let mut readiness = spec
            .health
            .as_ref()
            .map(|policy| ProbeTracker::new(policy, &record))
            .transpose()?;
        let mut liveness = spec
            .service_lifecycle
            .as_ref()
            .map(|lifecycle| ProbeTracker::new(&lifecycle.liveness, &record))
            .transpose()?;

        loop {
            record = self.refresh_record(spec, record).await?;
            if local_identity(&record)?.2 != ManagedExecutionState::Running {
                return self.observation(spec, &record, None, None).await;
            }
            if trackers_settled(&readiness, &liveness) {
                return self
                    .service_health_observation(spec, &record, readiness, liveness)
                    .await;
            }

            let now = tokio::time::Instant::now();
            let readiness_policy = readiness
                .as_ref()
                .filter(|tracker| tracker.is_due(now))
                .map(|tracker| tracker.policy);
            let liveness_policy = liveness
                .as_ref()
                .filter(|tracker| tracker.is_due(now))
                .map(|tracker| tracker.policy);

            if readiness_policy.is_none() && liveness_policy.is_none() {
                let wait = next_probe_wait(now, &readiness, &liveness)
                    .unwrap_or(self.config.task_poll_interval)
                    .min(self.config.task_poll_interval);
                tokio::time::sleep(wait).await;
                continue;
            }

            let (readiness_sample, liveness_sample) = match (readiness_policy, liveness_policy) {
                (Some(readiness_policy), Some(liveness_policy)) => {
                    let (readiness, liveness) = tokio::join!(
                        self.probe_health(spec, &record, readiness_policy),
                        self.probe_health(spec, &record, liveness_policy),
                    );
                    (Some(readiness?), Some(liveness?))
                }
                (Some(readiness_policy), None) => (
                    Some(self.probe_health(spec, &record, readiness_policy).await?),
                    None,
                ),
                (None, Some(liveness_policy)) => (
                    None,
                    Some(self.probe_health(spec, &record, liveness_policy).await?),
                ),
                (None, None) => {
                    return Err(RuntimeError::Protocol(
                        "Box health scheduler selected no due probe".into(),
                    ));
                }
            };

            // A Service can terminate while one or both bounded probes are in
            // flight. Refresh before publishing either result so stale health
            // can never make a stopped execution look ready or live.
            record = self.refresh_record(spec, record).await?;
            if local_identity(&record)?.2 != ManagedExecutionState::Running {
                return self.observation(spec, &record, None, None).await;
            }
            if let Some(sample) = readiness_sample {
                readiness
                    .as_mut()
                    .ok_or_else(|| {
                        RuntimeError::Protocol(
                            "Box produced readiness without a readiness tracker".into(),
                        )
                    })?
                    .record(sample);
            }
            if let Some(sample) = liveness_sample {
                liveness
                    .as_mut()
                    .ok_or_else(|| {
                        RuntimeError::Protocol(
                            "Box produced liveness without a liveness tracker".into(),
                        )
                    })?
                    .record(sample);
            }
        }
    }

    async fn service_health_observation(
        &self,
        spec: &RuntimeUnitSpec,
        record: &BoxRecord,
        readiness: Option<ProbeTracker<'_>>,
        liveness: Option<ProbeTracker<'_>>,
    ) -> RuntimeResult<RuntimeObservation> {
        let mut observation = self.build_observation(spec, record, None, None).await?;
        observation.health = readiness.and_then(|tracker| tracker.settled);
        observation.liveness = liveness.and_then(|tracker| tracker.settled);
        self.finish_observation(spec, record, observation).await
    }

    async fn probe_health(
        &self,
        spec: &RuntimeUnitSpec,
        record: &BoxRecord,
        policy: &RuntimeHealthCheck,
    ) -> RuntimeResult<RuntimeHealthObservation> {
        let timeout = Duration::from_millis(policy.timeout_ms);
        let (state, message) = match &policy.probe {
            HealthProbe::Http {
                port,
                path,
                expected_statuses,
            } => match tokio::time::timeout(
                timeout,
                self.http_probe(spec, record, port, path, timeout),
            )
            .await
            {
                Ok(Ok(status)) if expected_statuses.contains(&status) => {
                    (RuntimeHealthState::Healthy, None)
                }
                Ok(Ok(status)) => (
                    RuntimeHealthState::Unhealthy,
                    Some(format!("HTTP probe returned status {status}")),
                ),
                Ok(Err(_error)) => {
                    #[cfg(test)]
                    eprintln!(
                        "R17 HTTP health probe error: unit_id={} error={_error}",
                        spec.unit_id
                    );
                    (
                        RuntimeHealthState::Unhealthy,
                        Some(probe_message("HTTP probe failed")),
                    )
                }
                Err(_) => {
                    #[cfg(test)]
                    eprintln!("R17 HTTP health probe timeout: unit_id={}", spec.unit_id);
                    (
                        RuntimeHealthState::Unhealthy,
                        Some(probe_message("HTTP probe timed out")),
                    )
                }
            },
            HealthProbe::Tcp { port } => {
                match tokio::time::timeout(
                    timeout,
                    self.connect_health_port(spec, record, port, timeout),
                )
                .await
                {
                    Ok(Ok(stream)) => {
                        drop(stream);
                        (RuntimeHealthState::Healthy, None)
                    }
                    Ok(Err(_error)) => {
                        #[cfg(test)]
                        eprintln!(
                            "R17 TCP health probe error: unit_id={} error={_error}",
                            spec.unit_id
                        );
                        (
                            RuntimeHealthState::Unhealthy,
                            Some(probe_message("TCP probe failed")),
                        )
                    }
                    Err(_) => {
                        #[cfg(test)]
                        eprintln!("R17 TCP health probe timeout: unit_id={}", spec.unit_id);
                        (
                            RuntimeHealthState::Unhealthy,
                            Some(probe_message("TCP probe timed out")),
                        )
                    }
                }
            }
            HealthProbe::Command { command } => {
                self.command_probe(spec, record, command, timeout).await?
            }
        };
        Ok(RuntimeHealthObservation {
            state,
            checked_at_ms: now_ms(),
            message,
        })
    }

    async fn http_probe(
        &self,
        spec: &RuntimeUnitSpec,
        record: &BoxRecord,
        port_name: &str,
        path: &str,
        timeout: Duration,
    ) -> RuntimeResult<u16> {
        let mut stream = self
            .connect_health_port(spec, record, port_name, timeout)
            .await?;
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: runtime-health\r\nConnection: close\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|error| {
                RuntimeError::ProviderUnavailable(format!(
                    "Box HTTP health request failed: {error}"
                ))
            })?;
        read_http_status(&mut stream).await
    }

    async fn connect_health_port(
        &self,
        spec: &RuntimeUnitSpec,
        record: &BoxRecord,
        port_name: &str,
        timeout: Duration,
    ) -> RuntimeResult<ExecutionPortStream> {
        let port = spec
            .network
            .ports
            .iter()
            .find(|port| port.name == port_name)
            .ok_or_else(|| {
                RuntimeError::Protocol(format!("health port {port_name:?} is not declared"))
            })?;
        if port.protocol != TransportProtocol::Tcp {
            return Err(RuntimeError::UnsupportedCapabilities(vec![
                "feature:ServiceUdp".into(),
            ]));
        }
        let port = NonZeroU16::new(port.container_port)
            .ok_or_else(|| RuntimeError::Protocol("health port is zero".into()))?;
        let (execution_id, generation, state) = local_identity(record)?;
        if state != ManagedExecutionState::Running {
            return Err(RuntimeError::ProviderUnavailable(format!(
                "Box execution {execution_id} is not running for a health probe"
            )));
        }
        self.port_connector
            .connect_port(&execution_id, generation, port, timeout)
            .await
            .map_err(|error| map_execution_error(&spec.unit_id, error))
    }

    async fn command_probe(
        &self,
        spec: &RuntimeUnitSpec,
        record: &BoxRecord,
        command: &[String],
        timeout: Duration,
    ) -> RuntimeResult<(RuntimeHealthState, Option<String>)> {
        let timeout_ms = u64::try_from(timeout.as_millis()).map_err(|_| {
            RuntimeError::InvalidRequest("health command timeout overflows u64 milliseconds".into())
        })?;
        let timeout_ns = timeout_ms
            .checked_mul(NANOS_PER_MILLISECOND)
            .ok_or_else(|| {
                RuntimeError::InvalidRequest(
                    "health command timeout overflows u64 nanoseconds".into(),
                )
            })?;
        let (execution_id, generation, state) = local_identity(record)?;
        if state != ManagedExecutionState::Running {
            return Err(RuntimeError::ProviderUnavailable(format!(
                "Box execution {execution_id} is not running for a health probe"
            )));
        }
        let request = ExecRequest {
            request_id: None,
            cmd: command.to_vec(),
            timeout_ns,
            env: Vec::new(),
            working_dir: None,
            rootfs: None,
            stdin: None,
            stdin_streaming: false,
            user: None,
            streaming: false,
        };
        match tokio::time::timeout(
            timeout,
            self.manager.execute(&execution_id, generation, request),
        )
        .await
        {
            Ok(Ok(output)) if output.exit_code == 0 => Ok((RuntimeHealthState::Healthy, None)),
            Ok(Ok(output)) => Ok((
                RuntimeHealthState::Unhealthy,
                Some(probe_message(format!(
                    "Command probe exited with code {}",
                    output.exit_code
                ))),
            )),
            Ok(Err(error @ ExecutionManagerError::InvalidRequest(_)))
            | Ok(Err(error @ ExecutionManagerError::NotFound(_)))
            | Ok(Err(error @ ExecutionManagerError::Conflict { .. }))
            | Ok(Err(error @ ExecutionManagerError::Internal(_))) => {
                Err(map_execution_error(&spec.unit_id, error))
            }
            Ok(Err(ExecutionManagerError::Unavailable(_))) => Ok((
                RuntimeHealthState::Unhealthy,
                Some(probe_message("Command probe failed")),
            )),
            Err(_) => Ok((
                RuntimeHealthState::Unhealthy,
                Some(probe_message("Command probe timed out")),
            )),
        }
    }
}

async fn read_http_status(stream: &mut ExecutionPortStream) -> RuntimeResult<u16> {
    let mut head = Vec::with_capacity(512);
    let mut buffer = [0_u8; 512];
    loop {
        if head.windows(2).any(|window| window == b"\r\n") {
            break;
        }
        if head.len() >= MAX_HTTP_RESPONSE_HEAD_BYTES {
            return Err(RuntimeError::Protocol(
                "Box HTTP health response head exceeds 8 KiB".into(),
            ));
        }
        let read = stream.read(&mut buffer).await.map_err(|error| {
            RuntimeError::ProviderUnavailable(format!("Box HTTP health response failed: {error}"))
        })?;
        if read == 0 {
            return Err(RuntimeError::ProviderUnavailable(
                "Box HTTP health response ended before its status line".into(),
            ));
        }
        let remaining = MAX_HTTP_RESPONSE_HEAD_BYTES.saturating_sub(head.len());
        head.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    parse_http_status(&head).map_err(RuntimeError::Protocol)
}

fn parse_http_status(response: &[u8]) -> Result<u16, String> {
    let line_end = response
        .windows(2)
        .position(|window| window == b"\r\n")
        .ok_or_else(|| "HTTP health response has no complete status line".to_string())?;
    let line = std::str::from_utf8(&response[..line_end])
        .map_err(|_| "HTTP health status line is not UTF-8".to_string())?;
    let mut fields = line.split_ascii_whitespace();
    let version = fields
        .next()
        .ok_or_else(|| "HTTP health response has no version".to_string())?;
    let status = fields
        .next()
        .ok_or_else(|| "HTTP health response has no status".to_string())?;
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || status.len() != 3
        || !status.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("HTTP health response status line is invalid".into());
    }
    status
        .parse::<u16>()
        .ok()
        .filter(|status| (100..=599).contains(status))
        .ok_or_else(|| "HTTP health response status is out of range".into())
}

fn trackers_settled(
    readiness: &Option<ProbeTracker<'_>>,
    liveness: &Option<ProbeTracker<'_>>,
) -> bool {
    readiness.as_ref().is_none_or(ProbeTracker::is_settled)
        && liveness.as_ref().is_none_or(ProbeTracker::is_settled)
}

fn next_probe_wait(
    now: tokio::time::Instant,
    readiness: &Option<ProbeTracker<'_>>,
    liveness: &Option<ProbeTracker<'_>>,
) -> Option<Duration> {
    readiness
        .iter()
        .chain(liveness.iter())
        .filter(|tracker| !tracker.is_settled())
        .map(|tracker| tracker.next_probe_at.saturating_duration_since(now))
        .min()
}

fn remaining_start_period(record: &BoxRecord, start_period_ms: u64) -> RuntimeResult<Duration> {
    let started_at = record.started_at.ok_or_else(|| {
        RuntimeError::Protocol("running Box Service has no start timestamp".into())
    })?;
    let started_at_ms = timestamp_ms(started_at)?;
    Ok(remaining_start_period_at(
        started_at_ms,
        now_ms(),
        start_period_ms,
    ))
}

fn remaining_start_period_at(
    started_at_ms: u64,
    observed_at_ms: u64,
    start_period_ms: u64,
) -> Duration {
    let elapsed = observed_at_ms.saturating_sub(started_at_ms);
    Duration::from_millis(start_period_ms.saturating_sub(elapsed))
}

fn probe_message(message: impl AsRef<str>) -> String {
    let value = message.as_ref().replace(['\0', '\r', '\n'], " ");
    let value = value.trim();
    if value.is_empty() {
        "health probe failed".into()
    } else {
        value.chars().take(4096).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_policy(success_threshold: u32, failure_threshold: u32) -> RuntimeHealthCheck {
        RuntimeHealthCheck {
            probe: HealthProbe::Command {
                command: vec!["/bin/true".into()],
            },
            interval_ms: 100,
            timeout_ms: 50,
            start_period_ms: 0,
            success_threshold,
            failure_threshold,
        }
    }

    fn sample(state: RuntimeHealthState) -> RuntimeHealthObservation {
        RuntimeHealthObservation {
            state,
            checked_at_ms: 1,
            message: None,
        }
    }

    #[test]
    fn probe_tracker_requires_consecutive_successes_and_resets_on_failure() {
        let policy = command_policy(2, 2);
        let mut tracker = ProbeTracker {
            policy: &policy,
            next_probe_at: tokio::time::Instant::now(),
            successes: 0,
            failures: 0,
            settled: None,
        };

        tracker.record(sample(RuntimeHealthState::Healthy));
        tracker.record(sample(RuntimeHealthState::Unhealthy));
        tracker.record(sample(RuntimeHealthState::Healthy));
        assert!(!tracker.is_settled());
        tracker.record(sample(RuntimeHealthState::Healthy));

        assert_eq!(
            tracker.settled.as_ref().map(|health| health.state),
            Some(RuntimeHealthState::Healthy)
        );
    }

    #[test]
    fn probe_tracker_settles_only_after_the_failure_threshold() {
        let policy = command_policy(2, 2);
        let mut tracker = ProbeTracker {
            policy: &policy,
            next_probe_at: tokio::time::Instant::now(),
            successes: 0,
            failures: 0,
            settled: None,
        };

        tracker.record(sample(RuntimeHealthState::Unhealthy));
        assert!(!tracker.is_settled());
        tracker.record(sample(RuntimeHealthState::Unhealthy));

        assert_eq!(
            tracker.settled.as_ref().map(|health| health.state),
            Some(RuntimeHealthState::Unhealthy)
        );
    }

    #[test]
    fn parses_only_bounded_http_status_lines() {
        assert_eq!(
            parse_http_status(b"HTTP/1.1 204 No Content\r\nHeader: value\r\n").unwrap(),
            204
        );
        assert!(parse_http_status(b"HTTP/2 200\r\n").is_err());
        assert!(parse_http_status(b"HTTP/1.1 099 Nope\r\n").is_err());
        assert!(parse_http_status(b"HTTP/1.1 nope\r\n").is_err());
        assert!(parse_http_status(b"HTTP/1.1 200").is_err());
    }

    #[test]
    fn start_period_is_derived_from_the_provider_start_time() {
        assert_eq!(
            remaining_start_period_at(1_000, 1_250, 500),
            Duration::from_millis(250)
        );
        assert!(remaining_start_period_at(1_000, 1_500, 500).is_zero());
        assert_eq!(
            remaining_start_period_at(2_000, 1_000, 500),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn probe_messages_are_single_line_and_bounded() {
        let message = format!(" secret\r\n{}\0", "x".repeat(5_000));
        let sanitized = probe_message(&message);
        assert!(!sanitized.contains(['\0', '\r', '\n']));
        assert_eq!(sanitized.chars().count(), 4096);
        assert_eq!(probe_message("\r\n"), "health probe failed");
    }

    #[test]
    fn millisecond_timeout_conversion_remains_bounded() {
        assert_eq!(
            Duration::from_millis(500).as_nanos(),
            u128::from(500 * NANOS_PER_MILLISECOND)
        );
    }
}
