use std::time::Instant;

use a3s_runtime::contract::{
    HealthProbe, NetworkMode, RestartPolicy, RuntimeHealthCheck, RuntimeHealthState,
    RuntimeInspection, RuntimePort, RuntimeServiceLifecycle, RuntimeUnitSpec, RuntimeUnitState,
    TransportProtocol,
};
use a3s_runtime::RuntimeClient;

use super::fixture::BoxRuntimeConformanceFixture;
use super::{require, Result};

const SEPARATED_HTTP_SERVICE: &str = concat!(
    "rm -f /tmp/r17-lifecycle-http-requests; ",
    "cat > /tmp/r17-lifecycle-http-handler <<'R17_LIFECYCLE_HANDLER'\n",
    "#!/bin/sh\n",
    "IFS= read -r request_line || exit 1\n",
    "request_line=${request_line%$(printf '\\r')}\n",
    "while IFS= read -r header; do\n",
    "  [ \"$header\" = \"$(printf '\\r')\" ] && break\n",
    "done\n",
    "printf '%s\\n' \"$request_line\" >> /tmp/r17-lifecycle-http-requests\n",
    "case \"$request_line\" in\n",
    "  'GET /ready HTTP/1.1') status='204 No Content' ;;\n",
    "  'GET /live HTTP/1.1') status='200 OK' ;;\n",
    "  *) status='503 Service Unavailable' ;;\n",
    "esac\n",
    "printf 'HTTP/1.1 %s\r\nContent-Length: 0\r\nConnection: close\r\n\r\n' \"$status\"\n",
    "R17_LIFECYCLE_HANDLER\n",
    "chmod 0700 /tmp/r17-lifecycle-http-handler; ",
    "exec nc -ll -p 18082 -e /tmp/r17-lifecycle-http-handler",
);

pub(super) async fn run(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    readiness_and_liveness_are_separate(fixture, client).await?;
    liveness_transitions_without_changing_readiness(fixture, client).await?;
    unhealthy_liveness_drives_restart_policy(fixture, client).await?;
    graceful_stop_finishes_inside_the_declared_grace(fixture, client).await?;
    grace_deadline_forces_a_non_cooperative_service(fixture, client).await
}

async fn readiness_and_liveness_are_separate(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let mut request = fixture
        .cases
        .service("lifecycle-separation", SEPARATED_HTTP_SERVICE);
    add_tcp_service_port(&mut request.spec, "lifecycle", 18_082);
    request.spec.health = Some(health_check(HealthProbe::Http {
        port: "lifecycle".into(),
        path: "/ready".into(),
        expected_statuses: vec![204],
    }));
    request.spec.service_lifecycle = Some(RuntimeServiceLifecycle {
        liveness: health_check(HealthProbe::Http {
            port: "lifecycle".into(),
            path: "/live".into(),
            expected_statuses: vec![200],
        }),
        shutdown_grace_seconds: 3,
    });

    let observation = client.apply(&request).await?;
    require_lifecycle_state(
        &observation,
        RuntimeHealthState::Healthy,
        RuntimeHealthState::Healthy,
        "separated HTTP probes",
    )?;
    let paths = client
        .exec(&fixture.cases.exec(
            "lifecycle-separation-paths",
            &request.spec,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                concat!(
                    "grep -Fqx 'GET /ready HTTP/1.1' /tmp/r17-lifecycle-http-requests && ",
                    "grep -Fqx 'GET /live HTTP/1.1' /tmp/r17-lifecycle-http-requests",
                )
                .into(),
            ],
            5_000,
        ))
        .await?;
    require(
        paths.exit_code == 0,
        "readiness and liveness did not use their independently declared HTTP paths",
    )?;
    require_lifecycle_state(
        &paths.observation,
        RuntimeHealthState::Healthy,
        RuntimeHealthState::Healthy,
        "separated HTTP exec observation",
    )?;
    fixture
        .remove_unit(client, &request.spec, "lifecycle-separation")
        .await
}

async fn liveness_transitions_without_changing_readiness(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let mut request = fixture.cases.service(
        "lifecycle-transition",
        "rm -f /tmp/r17-live /tmp/r17-live-count; exec sleep 3600",
    );
    request.spec.health = Some(health_check(command_probe("exit 0")));
    request.spec.service_lifecycle = Some(RuntimeServiceLifecycle {
        liveness: RuntimeHealthCheck {
            failure_threshold: 2,
            ..health_check(command_probe(
                "count=$(cat /tmp/r17-live-count 2>/dev/null || printf '0'); count=$((count + 1)); printf '%s' \"$count\" > /tmp/r17-live-count; test -f /tmp/r17-live",
            ))
        },
        shutdown_grace_seconds: 3,
    });

    let initial = client.apply(&request).await?;
    require_lifecycle_state(
        &initial,
        RuntimeHealthState::Healthy,
        RuntimeHealthState::Unhealthy,
        "initial liveness transition",
    )?;
    let transition = client
        .exec(&fixture.cases.exec(
            "lifecycle-transition-live",
            &request.spec,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "cat /tmp/r17-live-count; touch /tmp/r17-live".into(),
            ],
            5_000,
        ))
        .await?;
    require(
        transition.stdout == "2",
        "liveness failure threshold did not require exactly two consecutive failures",
    )?;
    require_lifecycle_state(
        &transition.observation,
        RuntimeHealthState::Healthy,
        RuntimeHealthState::Healthy,
        "healthy liveness transition",
    )?;
    fixture
        .remove_unit(client, &request.spec, "lifecycle-transition")
        .await
}

async fn unhealthy_liveness_drives_restart_policy(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let mut request = fixture
        .cases
        .service("lifecycle-restart", "exec sleep 3600");
    request.spec.restart = RestartPolicy::Always;
    request.spec.health = Some(health_check(command_probe("exit 0")));
    request.spec.service_lifecycle = Some(RuntimeServiceLifecycle {
        // The root filesystem is recreated by a real Sandbox restart. Keep the
        // one-shot probe marker in Box's per-execution workspace so the test
        // verifies recovery instead of depending on ephemeral rootfs state.
        liveness: health_check(command_probe(concat!(
            "if [ -f /workspace/r17-liveness-restart-seen ]; then exit 0; fi; ",
            "touch /workspace/r17-liveness-restart-seen; exit 1",
        ))),
        shutdown_grace_seconds: 3,
    });

    let observation = client.apply(&request).await?;
    require_lifecycle_state(
        &observation,
        RuntimeHealthState::Healthy,
        RuntimeHealthState::Healthy,
        "liveness restart",
    )?;
    let record = fixture.record_for(&request.spec).await?;
    require(
        record
            .managed_execution
            .as_ref()
            .is_some_and(|metadata| metadata.generation.get() == 2),
        "unhealthy liveness did not advance the durable Box execution generation exactly once",
    )?;
    fixture
        .remove_unit(client, &request.spec, "lifecycle-restart")
        .await
}

async fn graceful_stop_finishes_inside_the_declared_grace(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let mut request = fixture.cases.service(
        "lifecycle-graceful-stop",
        "trap 'sleep 1; exit 0' TERM; while :; do sleep 1; done",
    );
    attach_command_lifecycle(&mut request.spec, 3);
    client.apply(&request).await?;

    let started = Instant::now();
    let stopped = client
        .stop(
            &fixture
                .cases
                .action("lifecycle-graceful-stop-action", &request.spec),
        )
        .await?;
    let elapsed = started.elapsed();
    require_stopped(stopped, "graceful stop")?;
    let record = fixture.record_for(&request.spec).await?;
    require(
        record.exit_code == Some(0),
        "cooperative Service did not preserve its graceful zero exit status",
    )?;
    require(
        elapsed.as_millis() >= 750 && elapsed.as_millis() < 3_500,
        format!("cooperative Service stopped outside its three-second grace: {elapsed:?}"),
    )?;
    fixture
        .remove_unit(client, &request.spec, "lifecycle-graceful-stop")
        .await
}

async fn grace_deadline_forces_a_non_cooperative_service(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let mut request = fixture.cases.service(
        "lifecycle-force-stop",
        "trap '' TERM; while :; do sleep 1; done",
    );
    attach_command_lifecycle(&mut request.spec, 1);
    client.apply(&request).await?;

    let started = Instant::now();
    let stopped = client
        .stop(
            &fixture
                .cases
                .action("lifecycle-force-stop-action", &request.spec),
        )
        .await?;
    let elapsed = started.elapsed();
    require_stopped(stopped, "forced stop")?;
    let record = fixture.record_for(&request.spec).await?;
    require(
        record.exit_code.is_some_and(|code| code != 0),
        "non-cooperative Service did not preserve forced-termination evidence",
    )?;
    require(
        elapsed.as_millis() >= 750 && elapsed.as_millis() < 3_000,
        format!("non-cooperative Service ignored its one-second grace: {elapsed:?}"),
    )?;
    fixture
        .remove_unit(client, &request.spec, "lifecycle-force-stop")
        .await
}

fn attach_command_lifecycle(spec: &mut RuntimeUnitSpec, shutdown_grace_seconds: u32) {
    spec.health = Some(health_check(command_probe("exit 0")));
    spec.service_lifecycle = Some(RuntimeServiceLifecycle {
        liveness: health_check(command_probe("exit 0")),
        shutdown_grace_seconds,
    });
}

fn add_tcp_service_port(spec: &mut RuntimeUnitSpec, name: &str, container_port: u16) {
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

fn health_check(probe: HealthProbe) -> RuntimeHealthCheck {
    RuntimeHealthCheck {
        probe,
        interval_ms: 200,
        timeout_ms: 150,
        start_period_ms: 0,
        success_threshold: 1,
        failure_threshold: 1,
    }
}

fn require_lifecycle_state(
    observation: &a3s_runtime::contract::RuntimeObservation,
    readiness: RuntimeHealthState,
    liveness: RuntimeHealthState,
    label: &str,
) -> Result<()> {
    require(
        observation.state == RuntimeUnitState::Running
            && observation
                .health
                .as_ref()
                .is_some_and(|health| health.state == readiness)
            && observation
                .liveness
                .as_ref()
                .is_some_and(|health| health.state == liveness),
        format!("{label} did not report readiness={readiness:?} and liveness={liveness:?}"),
    )
}

fn require_stopped(inspection: RuntimeInspection, label: &str) -> Result<()> {
    require(
        matches!(
            inspection,
            RuntimeInspection::Found { ref observation, .. }
                if matches!(observation.state, RuntimeUnitState::Stopped | RuntimeUnitState::Failed)
                    && observation.health.is_none()
                    && observation.liveness.is_none()
        ),
        format!("{label} did not return a terminal observation without health evidence"),
    )
}
