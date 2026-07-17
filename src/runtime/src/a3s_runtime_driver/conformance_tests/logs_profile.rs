use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use a3s_box_core::log::LogEntry;
use a3s_runtime::contract::{RuntimeLogChunk, RuntimeLogStream};
use a3s_runtime::{RuntimeClient, RuntimeError};

use super::fixture::BoxRuntimeConformanceFixture;
use super::{require, Result};

pub(super) async fn run(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let service = fixture.cases.service(
        "logs-service",
        "printf 'r17-log-stdout\\n'; printf 'r17-log-stderr\\n' >&2; exec sleep 3600",
    );
    client.apply(&service).await?;
    let initial = wait_for_initial_logs(fixture, client, &service.spec).await?;
    require(
        initial
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence),
        "Box logs are not in strict total order",
    )?;
    let stdout = initial
        .iter()
        .find(|chunk| chunk.data.contains("r17-log-stdout"))
        .ok_or_else(|| super::protocol("Box logs omitted stdout"))?;
    let stderr = initial
        .iter()
        .find(|chunk| chunk.data.contains("r17-log-stderr"))
        .ok_or_else(|| super::protocol("Box logs omitted stderr"))?;
    require(
        stdout.stream == RuntimeLogStream::Stdout && stderr.stream == RuntimeLogStream::Stderr,
        "Box log streams were mislabeled",
    )?;

    let stderr_only = client
        .logs(
            &fixture
                .cases
                .logs(&service.spec, None, 100, Some(RuntimeLogStream::Stderr)),
        )
        .await?;
    require(
        !stderr_only.is_empty()
            && stderr_only
                .iter()
                .all(|chunk| chunk.stream == RuntimeLogStream::Stderr),
        "Box stderr filter leaked another stream",
    )?;
    let limited = client
        .logs(&fixture.cases.logs(&service.spec, None, 1, None))
        .await?;
    require(limited.len() == 1, "Box log limit was not enforced")?;
    let resumed = client
        .logs(
            &fixture
                .cases
                .logs(&service.spec, Some(initial[0].cursor.clone()), 100, None),
        )
        .await?;
    require(
        resumed.first().map(|chunk| chunk.cursor.as_str())
            == initial.get(1).map(|chunk| chunk.cursor.as_str()),
        "Box log cursor did not resume after the addressed record",
    )?;

    let stop = fixture.cases.action("logs-service-stop", &service.spec);
    client.stop(&stop).await?;
    let retained = client
        .logs(&fixture.cases.logs(&service.spec, None, 100, None))
        .await?;
    require(
        retained
            .iter()
            .any(|chunk| chunk.data.contains("r17-log-stdout")),
        "stopped Service did not retain its logs",
    )?;

    let record = fixture.record_for(&service.spec).await?;
    let structured = record.box_dir.join("logs/container.json");
    append_entries(
        &structured,
        &[
            LogEntry {
                log: "r17-same-time-one\\n".into(),
                stream: "stdout".into(),
                time: "2026-07-17T12:00:00.123456789Z".into(),
            },
            LogEntry {
                log: "r17-same-time-two\\n".into(),
                stream: "stderr".into(),
                time: "2026-07-17T12:00:00.123456789Z".into(),
            },
        ],
    )?;
    let same_time = client
        .logs(&fixture.cases.logs(&service.spec, None, 10_000, None))
        .await?;
    let same_time = same_time
        .iter()
        .filter(|chunk| chunk.data.starts_with("r17-same-time-"))
        .collect::<Vec<_>>();
    require(
        same_time.len() == 2 && same_time[0].sequence < same_time[1].sequence,
        "same-timestamp Box log records lost total order",
    )?;
    let rotation_cursor = same_time[0].cursor.clone();

    write_entries(
        &structured,
        &[LogEntry {
            log: "r17-after-rotation\\n".into(),
            stream: "stdout".into(),
            time: "2026-07-17T12:00:01Z".into(),
        }],
    )?;
    let gap = client
        .logs(
            &fixture
                .cases
                .logs(&service.spec, Some(rotation_cursor), 100, None),
        )
        .await
        .unwrap_err();
    require(
        matches!(gap, RuntimeError::Protocol(ref message) if message.contains("rotation gap")),
        format!("missing Box log cursor did not report a rotation gap: {gap}"),
    )?;

    write_entries(
        &structured,
        &[LogEntry {
            log: "x".repeat(1024 * 1024 + 1),
            stream: "stdout".into(),
            time: "2026-07-17T12:00:02Z".into(),
        }],
    )?;
    let oversized = client
        .logs(&fixture.cases.logs(&service.spec, None, 100, None))
        .await
        .unwrap_err();
    require(
        matches!(oversized, RuntimeError::Protocol(ref message) if message.contains("one-MiB")),
        format!("oversized Box log record did not fail closed: {oversized}"),
    )?;

    fixture
        .remove_unit(client, &service.spec, "logs-service")
        .await?;
    let removed = client
        .logs(&fixture.cases.logs(&service.spec, None, 100, None))
        .await;
    require(
        matches!(removed, Err(RuntimeError::NotFound { .. })),
        "removed Service still exposed provider logs",
    )
}

async fn wait_for_initial_logs(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
    spec: &a3s_runtime::contract::RuntimeUnitSpec,
) -> Result<Vec<RuntimeLogChunk>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let chunks = client
            .logs(&fixture.cases.logs(spec, None, 100, None))
            .await?;
        if chunks
            .iter()
            .any(|chunk| chunk.data.contains("r17-log-stdout"))
            && chunks
                .iter()
                .any(|chunk| chunk.data.contains("r17-log-stderr"))
        {
            return Ok(chunks);
        }
        if Instant::now() >= deadline {
            return Err(super::protocol(
                "Box log worker did not publish both streams within five seconds",
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn append_entries(path: &Path, entries: &[LogEntry]) -> Result<()> {
    let mut output = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| super::external("open structured log for append", error))?;
    write_to(&mut output, entries)
}

fn write_entries(path: &Path, entries: &[LogEntry]) -> Result<()> {
    let mut output = std::fs::File::create(path)
        .map_err(|error| super::external("replace structured log", error))?;
    write_to(&mut output, entries)
}

fn write_to(output: &mut std::fs::File, entries: &[LogEntry]) -> Result<()> {
    for entry in entries {
        serde_json::to_writer(&mut *output, entry)
            .map_err(|error| super::external("encode structured log entry", error))?;
        output
            .write_all(b"\n")
            .map_err(|error| super::external("write structured log entry", error))?;
    }
    output
        .sync_all()
        .map_err(|error| super::external("sync structured log entries", error))
}
