use a3s_runtime::contract::{
    HealthProbe, NetworkMode, RuntimeHealthCheck, RuntimeHealthState, RuntimePort,
    RuntimeUnitState, TransportProtocol,
};
use a3s_runtime::RuntimeClient;

use super::fixture::BoxRuntimeConformanceFixture;
use super::{require, Result};

const HTTP_HEALTH_SERVICE: &str = concat!(
    "rm -f /tmp/r17-health-http-request; ",
    "while :; do ",
    // Keep netcat's stdin open after sending the response so BusyBox cannot
    // exit on local EOF before it has consumed the request from the socket.
    "{ printf 'HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n'; sleep 1; } ",
    "| nc -l -p 18080 >> /tmp/r17-health-http-request; ",
    "done",
);

pub(super) async fn run(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    http_probe_reaches_the_generation_fenced_service(fixture, client).await?;
    tcp_probe_reaches_the_generation_fenced_service(fixture, client).await?;
    command_threshold_transitions_from_unhealthy_to_healthy(fixture, client).await?;
    command_probe_timeout_is_bounded(fixture, client).await?;
    start_period_defers_failure_accounting(fixture, client).await?;
    service_exit_wins_over_an_in_flight_probe(fixture, client).await
}

async fn http_probe_reaches_the_generation_fenced_service(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let mut request = fixture.cases.service("health-http", HTTP_HEALTH_SERVICE);
    add_tcp_service_port(&mut request.spec, "health", 18_080);
    request.spec.health = Some(health_check(
        HealthProbe::Http {
            port: "health".into(),
            path: "/ready".into(),
            expected_statuses: vec![204],
        },
        200,
        150,
        0,
        1,
        20,
    ));

    let observation = client.apply(&request).await?;
    require_healthy(&observation, "HTTP")?;
    let request_line = client
        .exec(&fixture.cases.exec(
            "health-http-request-line",
            &request.spec,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                concat!(
                    "i=0; ",
                    "while [ ! -s /tmp/r17-health-http-request ] && [ \"$i\" -lt 50 ]; do ",
                    "sleep 0.1; i=$((i + 1)); ",
                    "done; ",
                    "[ -s /tmp/r17-health-http-request ] || exit 17; ",
                    // The first one-shot netcat exits after the startup probe.
                    // Wait for the loop to bind its successor before exec's
                    // post-command health observation opens a second stream.
                    "i=0; ",
                    "until awk '$2 ~ /:46A0$/ && $4 == \"0A\" { found=1 } END { exit found ? 0 : 1 }' /proc/net/tcp; do ",
                    "[ \"$i\" -lt 50 ] || exit 18; ",
                    "sleep 0.1; i=$((i + 1)); ",
                    "done; ",
                    "head -n 1 /tmp/r17-health-http-request | tr -d '\\r'",
                )
                .into(),
            ],
            5_000,
        ))
        .await?;
    require(
        request_line.exit_code == 0 && request_line.stdout == "GET /ready HTTP/1.1\n",
        "HTTP health probe did not send the declared request path",
    )?;
    require_healthy(&request_line.observation, "HTTP exec observation")?;
    fixture
        .remove_unit(client, &request.spec, "health-http")
        .await
}

async fn tcp_probe_reaches_the_generation_fenced_service(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let mut request = fixture.cases.service(
        "health-tcp",
        "while :; do printf 'r17-health-tcp\n' | nc -l -p 18081; done",
    );
    add_tcp_service_port(&mut request.spec, "health", 18_081);
    request.spec.health = Some(health_check(
        HealthProbe::Tcp {
            port: "health".into(),
        },
        200,
        150,
        0,
        1,
        20,
    ));

    let observation = client.apply(&request).await?;
    require_healthy(&observation, "TCP")?;
    fixture
        .remove_unit(client, &request.spec, "health-tcp")
        .await
}

async fn command_threshold_transitions_from_unhealthy_to_healthy(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let mut request = fixture.cases.service(
        "health-command-transition",
        "rm -f /tmp/r17-health-ready /tmp/r17-health-count; exec sleep 3600",
    );
    request.spec.health = Some(health_check(
        command_probe(
            "count=$(cat /tmp/r17-health-count 2>/dev/null || printf '0'); count=$((count + 1)); printf '%s' \"$count\" > /tmp/r17-health-count; printf 'health-output-must-not-escape\\n'; test -f /tmp/r17-health-ready",
        ),
        200,
        150,
        0,
        1,
        2,
    ));

    let initial = client.apply(&request).await?;
    require_health_state(
        &initial,
        RuntimeHealthState::Unhealthy,
        "Command failure threshold",
    )?;
    require(
        initial
            .health
            .as_ref()
            .and_then(|health| health.message.as_deref())
            .is_some_and(|message| {
                message == "Command probe exited with code 1"
                    && !message.contains("health-output-must-not-escape")
            }),
        "Command health observation exposed probe output or lost the exit status",
    )?;

    let transition = client
        .exec(&fixture.cases.exec(
            "health-command-transition-ready",
            &request.spec,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "cat /tmp/r17-health-count; touch /tmp/r17-health-ready".into(),
            ],
            5_000,
        ))
        .await?;
    require(
        transition.stdout == "2",
        "Command health failure threshold did not execute exactly two failing probes",
    )?;
    require_healthy(&transition.observation, "Command")?;
    fixture
        .remove_unit(client, &request.spec, "health-command-transition")
        .await
}

async fn command_probe_timeout_is_bounded(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let mut request = fixture
        .cases
        .service("health-command-timeout", "exec sleep 3600");
    request.spec.health = Some(health_check(
        command_probe("exec sleep 3600"),
        150,
        150,
        0,
        1,
        1,
    ));

    let observation = client.apply(&request).await?;
    require(
        health_elapsed_ms(&observation).is_some_and(|elapsed| elapsed < 5_000),
        "Command health probe ignored its timeout",
    )?;
    require_health_state(
        &observation,
        RuntimeHealthState::Unhealthy,
        "Command timeout",
    )?;
    fixture
        .remove_unit(client, &request.spec, "health-command-timeout")
        .await
}

async fn start_period_defers_failure_accounting(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let mut request = fixture.cases.service(
        "health-start-period",
        "rm -f /tmp/r17-health-ready /tmp/r17-health-success-count; (sleep 1; touch /tmp/r17-health-ready) & exec sleep 3600",
    );
    request.spec.health = Some(health_check(
        command_probe(
            "count=$(cat /tmp/r17-health-success-count 2>/dev/null || printf '0'); count=$((count + 1)); printf '%s' \"$count\" > /tmp/r17-health-success-count; test -f /tmp/r17-health-ready",
        ),
        200,
        150,
        1_200,
        2,
        1,
    ));

    let observation = client.apply(&request).await?;
    require(
        health_elapsed_ms(&observation).is_some_and(|elapsed| elapsed >= 1_100),
        "Health start period was not measured from provider startup",
    )?;
    require_healthy(&observation, "start-period Command")?;
    let success_count = client
        .exec(&fixture.cases.exec(
            "health-start-period-success-count",
            &request.spec,
            vec!["cat".into(), "/tmp/r17-health-success-count".into()],
            5_000,
        ))
        .await?;
    require(
        success_count.stdout == "2",
        "Command health success threshold did not require two consecutive probes",
    )?;
    require_healthy(&success_count.observation, "start-period exec observation")?;
    fixture
        .remove_unit(client, &request.spec, "health-start-period")
        .await
}

async fn service_exit_wins_over_an_in_flight_probe(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let mut request = fixture
        .cases
        .service("health-service-exit", "sleep 1; exit 17");
    request.spec.health = Some(health_check(
        command_probe("exec sleep 5"),
        3_000,
        3_000,
        0,
        1,
        1,
    ));

    let observation = client.apply(&request).await?;
    require(
        matches!(
            observation.state,
            RuntimeUnitState::Stopped | RuntimeUnitState::Failed
        ) && observation.health.is_none(),
        "A stale in-flight health probe hid the Service exit",
    )?;
    fixture
        .remove_unit(client, &request.spec, "health-service-exit")
        .await
}

fn add_tcp_service_port(
    spec: &mut a3s_runtime::contract::RuntimeUnitSpec,
    name: &str,
    container_port: u16,
) {
    spec.network.mode = NetworkMode::Service;
    spec.network.ports = vec![RuntimePort {
        name: name.into(),
        container_port,
        protocol: TransportProtocol::Tcp,
    }];
}

fn command_probe(script: &str) -> HealthProbe {
    HealthProbe::Command {
        command: vec!["/bin/sh".into(), "-c".into(), script.into()],
    }
}

fn health_check(
    probe: HealthProbe,
    interval_ms: u64,
    timeout_ms: u64,
    start_period_ms: u64,
    success_threshold: u32,
    failure_threshold: u32,
) -> RuntimeHealthCheck {
    RuntimeHealthCheck {
        probe,
        interval_ms,
        timeout_ms,
        start_period_ms,
        success_threshold,
        failure_threshold,
    }
}

fn require_healthy(
    observation: &a3s_runtime::contract::RuntimeObservation,
    label: &str,
) -> Result<()> {
    require(
        observation.state == RuntimeUnitState::Running,
        format!("{label} health Service did not remain running"),
    )?;
    require_health_state(observation, RuntimeHealthState::Healthy, label)
}

fn require_health_state(
    observation: &a3s_runtime::contract::RuntimeObservation,
    expected: RuntimeHealthState,
    label: &str,
) -> Result<()> {
    require(
        observation
            .health
            .as_ref()
            .is_some_and(|health| health.state == expected),
        format!("{label} health observation did not report {expected:?}"),
    )
}

fn health_elapsed_ms(observation: &a3s_runtime::contract::RuntimeObservation) -> Option<u64> {
    observation.health.as_ref().and_then(|health| {
        observation
            .started_at_ms
            .map(|started| health.checked_at_ms.saturating_sub(started))
    })
}
