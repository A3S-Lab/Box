use a3s_runtime::contract::RuntimeUnitState;
use a3s_runtime::{RuntimeClient, RuntimeError};

use super::fixture::BoxRuntimeConformanceFixture;
use super::{require, Result};

pub(super) async fn run(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let service = fixture.cases.service(
        "exec-service",
        "printf 'r17-exec-ready\\n'; exec sleep 3600",
    );
    let running = client.apply(&service).await?;
    require(
        running.state == RuntimeUnitState::Running,
        "exec fixture Service did not reach running",
    )?;

    let basic_request = fixture.cases.exec(
        "exec-exit-code",
        &service.spec,
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'r17-exec-stdout\\n'; printf 'r17-exec-stderr\\n' >&2; exit 23".into(),
        ],
        5_000,
    );
    let basic = client.exec(&basic_request).await?;
    require(
        basic.exit_code == 23
            && basic.stdout == "r17-exec-stdout\n"
            && basic.stderr == "r17-exec-stderr\n"
            && !basic.truncated,
        "Box exec did not preserve exit code and stream identity",
    )?;
    let replay = client.exec(&basic_request).await?;
    require(replay == basic, "Box exec exact replay changed its result")?;

    let mut conflict = basic_request.clone();
    conflict.command = vec!["/bin/true".into()];
    require(
        matches!(
            client.exec(&conflict).await,
            Err(RuntimeError::RequestConflict { .. })
        ),
        "Box exec accepted conflicting content for one request ID",
    )?;

    let timeout_request = fixture.cases.exec(
        "exec-timeout",
        &service.spec,
        vec!["/bin/sh".into(), "-c".into(), "exec sleep 3600".into()],
        150,
    );
    let timeout = client.exec(&timeout_request).await?;
    require(
        timeout.exit_code == 137 && timeout.stderr.contains("timeout exceeded"),
        "Box exec timeout did not kill and report the command",
    )?;
    require(
        client.exec(&timeout_request).await? == timeout,
        "timed-out Box exec was re-executed instead of replayed",
    )?;

    let output = client
        .exec(&fixture.cases.exec(
            "exec-output-bounds",
            &service.spec,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "yes o | head -c 1100000; yes e | head -c 1100000 >&2".into(),
            ],
            15_000,
        ))
        .await?;
    require(
        output.truncated
            && output.stdout.len() == 1024 * 1024
            && output.stderr.len() == 1024 * 1024,
        "Box exec did not enforce the one-MiB per-stream output bound",
    )?;

    let mut wrong_generation = fixture.cases.exec(
        "exec-wrong-generation",
        &service.spec,
        vec!["/bin/true".into()],
        5_000,
    );
    wrong_generation.generation += 1;
    require(
        client.exec(&wrong_generation).await.is_err(),
        "Box exec accepted a request for another generation",
    )?;

    let stop = fixture.cases.action("exec-service-stop", &service.spec);
    client.stop(&stop).await?;
    let stopped_exec = fixture.cases.exec(
        "exec-stopped-state",
        &service.spec,
        vec!["/bin/true".into()],
        5_000,
    );
    require(
        matches!(
            client.exec(&stopped_exec).await,
            Err(RuntimeError::InvalidRequest(_))
        ),
        "Box exec accepted a stopped Service",
    )?;
    fixture
        .remove_unit(client, &service.spec, "exec-service")
        .await
}
