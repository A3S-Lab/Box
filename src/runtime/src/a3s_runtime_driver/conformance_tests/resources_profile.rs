use std::path::Path;
use std::time::{Duration, Instant};

use a3s_runtime::contract::{RuntimeInspection, RuntimeUnitState};
use a3s_runtime::RuntimeClient;

use super::cases::ResourceShape;
use super::fixture::BoxRuntimeConformanceFixture;
use super::{require, Result};

const CPU_MILLIS: u64 = 100;
const CPU_PERIOD_US: u64 = 100_000;
const CPU_QUOTA_US: u64 = CPU_MILLIS * (CPU_PERIOD_US / 1_000);
const MEMORY_BYTES: u64 = 128 * 1024 * 1024;
const PIDS: u32 = 32;

pub(super) async fn run(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let service = fixture.cases.apply(
        "resources-service",
        a3s_runtime::contract::RuntimeUnitClass::Service,
        "printf 'r17-resources-ready\\n'; exec sleep 3600",
        ResourceShape {
            cpu_millis: CPU_MILLIS,
            memory_bytes: MEMORY_BYTES,
            pids: PIDS,
            execution_timeout_ms: None,
        },
        a3s_runtime::contract::RestartPolicy::Never,
    );
    let running = client.apply(&service).await?;
    require(
        running.state == RuntimeUnitState::Running,
        "resource fixture Service did not reach running",
    )?;
    let record = fixture.record_for(&service.spec).await?;
    let config = &record
        .managed_execution
        .as_ref()
        .ok_or_else(|| super::protocol("resource fixture lost managed metadata"))?
        .request
        .config;
    require(
        config.resource_limits.cpu_quota == Some(CPU_QUOTA_US as i64)
            && config.resource_limits.cpu_period == Some(CPU_PERIOD_US),
        "CPU limits changed before provider launch",
    )?;
    require(
        config.resource_limits.sandbox_memory_limit_bytes == Some(MEMORY_BYTES)
            && config.resource_limits.memory_swap == Some(MEMORY_BYTES as i64),
        "memory limits changed before provider launch",
    )?;
    require(
        config.resource_limits.pids_limit == Some(u64::from(PIDS)),
        "PID limit changed before provider launch",
    )?;

    let cgroup = Path::new("/sys/fs/cgroup/a3s-box").join(&record.id);
    require(cgroup.is_dir(), "Sandbox cgroup was not created")?;
    require(
        read_trimmed(&cgroup.join("cpu.max"))? == format!("{CPU_QUOTA_US} {CPU_PERIOD_US}"),
        "cpu.max does not match the Runtime CPU request",
    )?;
    require(
        read_trimmed(&cgroup.join("memory.max"))? == MEMORY_BYTES.to_string(),
        "memory.max does not match the exact Runtime byte limit",
    )?;
    require(
        read_trimmed(&cgroup.join("pids.max"))? == PIDS.to_string(),
        "pids.max does not match the Runtime PID request",
    )?;

    let visible = client
        .exec(&fixture.cases.exec(
            "resources-visible-config",
            &service.spec,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "cat /sys/fs/cgroup/cpu.max; cat /sys/fs/cgroup/memory.max; cat /sys/fs/cgroup/pids.max"
                    .into(),
            ],
            5_000,
        ))
        .await?;
    require(
        visible
            .stdout
            .contains(&format!("{CPU_QUOTA_US} {CPU_PERIOD_US}"))
            && visible.stdout.contains(&MEMORY_BYTES.to_string())
            && visible.stdout.contains(&PIDS.to_string()),
        "workload did not observe its configured cgroup limits",
    )?;

    let throttled_before = counter(&cgroup.join("cpu.stat"), "nr_throttled")?;
    let cpu = client
        .exec(&fixture.cases.exec(
            "resources-cpu-behavior",
            &service.spec,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "timeout 1 sh -c 'trap \"exit 124\" TERM; while :; do :; done'".into(),
            ],
            5_000,
        ))
        .await?;
    require(
        cpu.exit_code == 124,
        format!(
            "CPU saturation oracle exited with {}; stdout={:?}; stderr={:?}",
            cpu.exit_code, cpu.stdout, cpu.stderr
        ),
    )?;
    let throttled_after = counter(&cgroup.join("cpu.stat"), "nr_throttled")?;
    require(
        throttled_after > throttled_before,
        "CPU workload was never throttled by the configured quota",
    )?;

    let pid_max_before = counter(&cgroup.join("pids.events"), "max")?;
    let _ = client
        .exec(&fixture.cases.exec(
            "resources-pids-behavior",
            &service.spec,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "i=0; while [ \"$i\" -lt 96 ]; do sleep 1 & i=$((i + 1)); done; wait".into(),
            ],
            15_000,
        ))
        .await?;
    let pid_max_after = counter(&cgroup.join("pids.events"), "max")?;
    require(
        pid_max_after > pid_max_before,
        "PID-heavy workload did not hit the configured pids.max",
    )?;

    let oom_before = counter(&cgroup.join("memory.events"), "oom_kill")?;
    let memory = client
        .exec(&fixture.cases.exec(
            "resources-memory-behavior",
            &service.spec,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "awk 'BEGIN { s=\"0123456789abcdef\"; for (i=0; i<25; i++) s=s s; print length(s) }'"
                    .into(),
            ],
            15_000,
        ))
        .await?;
    let oom_after = counter(&cgroup.join("memory.events"), "oom_kill")?;
    require(
        oom_after > oom_before && memory.exit_code != 0,
        "memory-heavy workload was not killed by the exact memory limit",
    )?;
    let RuntimeInspection::Found { observation, .. } =
        client.inspect(&service.spec.unit_id).await?
    else {
        return Err(super::protocol(
            "resource probes lost the long-running Service",
        ));
    };
    require(
        observation.state == RuntimeUnitState::Running,
        "resource-limit probes killed the Sandbox Service",
    )?;

    let timeout = fixture
        .cases
        .task("resources-execution-timeout", "exec sleep 3600", 400);
    let started = Instant::now();
    let timed_out = client.apply(&timeout).await?;
    require(
        timed_out.state == RuntimeUnitState::Failed
            && timed_out
                .failure
                .as_ref()
                .is_some_and(|failure| failure.code == "execution_timeout" && !failure.retryable)
            && started.elapsed() < Duration::from_secs(5),
        "Task execution timeout was not behaviorally enforced",
    )?;
    fixture
        .remove_unit(client, &timeout.spec, "resources-execution-timeout")
        .await?;
    fixture
        .remove_unit(client, &service.spec, "resources-service")
        .await
}

fn read_trimmed(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .map(|value| value.trim().to_string())
        .map_err(|error| super::external(&format!("read {}", path.display()), error))
}

fn counter(path: &Path, key: &str) -> Result<u64> {
    let value = read_trimmed(path)?;
    value
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some(key))
                .then(|| fields.next()?.parse::<u64>().ok())
                .flatten()
        })
        .ok_or_else(|| {
            super::protocol(format!(
                "cgroup counter {key:?} is missing from {}",
                path.display()
            ))
        })
}
