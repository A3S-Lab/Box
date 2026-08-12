use super::*;

#[tokio::test]
async fn emits_only_selected_oci_resource_fields() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let service = Arc::new(FakeRuntimeService::launch_ready());
    let manager = manager(
        &directory,
        test_endpoint(),
        service.clone(),
        Arc::new(FakeBundleProvider::default()),
    );
    let lease = manager
        .create_and_start(
            request("partial-resource-update", ExecutionIsolation::Sandbox),
            &box_operation("partial-resource-update-create"),
        )
        .await
        .expect("initial launch");

    manager
        .update_resources(
            &lease.execution_id,
            lease.generation,
            &box_operation("partial-resource-update-live"),
            ExecutionResourceUpdate {
                cpu_shares: Some(1_024),
                ..Default::default()
            },
        )
        .await
        .expect("CPU shares update");

    let requests = service.update_requests();
    assert_eq!(requests.len(), 1);
    let resources = &requests[0].resources;
    assert!(resources.memory().is_none());
    assert!(resources.pids().is_none());
    assert!(resources.devices().is_none());
    let cpu = resources.cpu().as_ref().expect("CPU resource update");
    assert_eq!(cpu.shares(), Some(1_024));
    assert_eq!(cpu.quota(), None);
    assert_eq!(cpu.period(), None);
    assert!(cpu.cpus().is_none());
}

#[test]
fn couples_derived_cpu_quota_with_period_updates() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let execution_id = ExecutionId::new("period-update").expect("execution ID");
    let mut request = request("period-update", ExecutionIsolation::Sandbox);
    request.config.resources.vcpus = 2;
    let record = build_managed_record(
        directory.path(),
        &execution_id,
        box_operation("period-update-create"),
        request,
        Utc::now(),
    )
    .expect("managed record");

    let resources = compile_resource_update(
        &record,
        &ExecutionResourceUpdate {
            cpu_period: Some(200_000),
            ..Default::default()
        },
    )
    .expect("CPU period update");
    let cpu = resources.cpu().as_ref().expect("CPU max update");
    assert_eq!(cpu.quota(), Some(400_000));
    assert_eq!(cpu.period(), Some(200_000));
    assert_eq!(cpu.shares(), None);
    assert!(resources.memory().is_none());
    assert!(resources.pids().is_none());
    assert!(resources.devices().is_none());
}
