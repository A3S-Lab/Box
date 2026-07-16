use a3s_box_core::{
    BoxConfig, ExecutionGeneration, ExecutionId, ExecutionIsolation, ExecutionSnapshotId,
    ResourceConfig,
};
use chrono::{DateTime, Duration, TimeZone, Utc};

use crate::control::{EnvdMode, PublicSandboxState, ResolvedTemplate, SandboxId};

use super::super::*;

pub fn test_time(second: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 16, 12, 0, 0)
        .single()
        .unwrap()
        + Duration::seconds(second)
}

pub fn template() -> ResolvedTemplate {
    ResolvedTemplate {
        config: BoxConfig {
            image: "alpine:3.20".to_string(),
            isolation: ExecutionIsolation::Sandbox,
            resources: ResourceConfig {
                vcpus: 2,
                memory_mb: 512,
                disk_mb: 1024,
                timeout: 300,
            },
            ..BoxConfig::default()
        },
        envd_version: "0.1.3".to_string(),
        envd_mode: EnvdMode::Broker,
        routing: crate::routing::SandboxRoutePolicy::default(),
        rootfs_snapshot_id: None,
    }
}

pub fn record(
    id: &str,
    owner: &str,
    name: Option<&str>,
    state: SnapshotState,
    second: i64,
) -> SnapshotRecord {
    let mut record = SnapshotRecord::creating(
        SnapshotId::new(id).unwrap(),
        ExecutionSnapshotId::new(format!("content-{id}")).unwrap(),
        owner,
        SandboxId::new(format!("sandbox-{id}")).unwrap(),
        ExecutionId::new(format!("execution-{id}")).unwrap(),
        ExecutionGeneration::INITIAL,
        PublicSandboxState::Running,
        name.map(str::to_string),
        "a3s-0123456789ab",
        template(),
        test_time(second),
    )
    .unwrap();
    if matches!(state, SnapshotState::Active | SnapshotState::Deleting) {
        record.mark_active(4_096).unwrap();
    }
    if state == SnapshotState::Deleting {
        record.begin_delete().unwrap();
    }
    record
}
