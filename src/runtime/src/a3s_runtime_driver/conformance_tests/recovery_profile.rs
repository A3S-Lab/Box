use std::sync::Arc;
use std::time::Duration;

use a3s_box_core::{ExecutionManager, OperationId};
use a3s_runtime::contract::{RuntimeInspection, RuntimeUnitState};
use a3s_runtime::{
    RuntimeClient, RuntimeDriver, RuntimeError, RuntimeRequestState, RuntimeStateStore,
};

use crate::ManagedExecutionState;

use super::super::mapping::{creation_request, operation};
use super::super::metadata::{local_identity, now_ms};
use super::fixture::BoxRuntimeConformanceFixture;
use super::{require, Result};

pub(super) async fn run(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    // Keep the real-provider scenario state machines off libtest's default
    // thread stack as this profile grows.
    Box::pin(partial_creation_replays_same_provider_identity(fixture)).await?;
    Box::pin(create_before_ack_and_client_restart(fixture)).await?;
    Box::pin(cancelled_task_apply_is_replayable(fixture)).await?;
    Box::pin(completed_client_restart(fixture, client)).await?;
    Box::pin(provider_restart(fixture, client)).await?;
    Box::pin(external_deletion_and_single_replacement(fixture, client)).await?;
    Box::pin(duplicate_resource_detection(fixture, client)).await
}

async fn partial_creation_replays_same_provider_identity(
    fixture: &BoxRuntimeConformanceFixture,
) -> Result<()> {
    let request = fixture.cases.service(
        "recovery-partial-creation",
        "printf 'r17-partial-creation\\n'; exec sleep 3600",
    );
    let state_reservation = fixture.state.reserve_apply(&request, now_ms()).await?;
    require(
        state_reservation.dispatch,
        "partial-creation reservation did not require provider work",
    )?;

    let provider_reservation = fixture
        .driver
        .manager
        .create(creation_request(&request.spec)?, &operation(&request.spec)?)
        .await
        .map_err(|error| super::external("reserve partial Sandbox creation", error))?;
    let provider_id = provider_reservation.execution_id.to_string();
    let created = fixture
        .driver
        .manager
        .managed_record(&provider_reservation.execution_id)
        .await
        .map_err(|error| super::external("load partial Sandbox creation", error))?
        .ok_or_else(|| super::protocol("partial Sandbox creation was not durable"))?;
    require(
        local_identity(&created)?.2 == ManagedExecutionState::Created,
        "partial Sandbox creation advanced before Runtime replay",
    )?;

    let restarted_driver = fixture.restarted_driver()?;
    let restarted = fixture.client_with(restarted_driver.clone(), fixture.state.clone());
    let recovered = restarted.apply(&request).await?;
    require(
        recovered.state == RuntimeUnitState::Running
            && recovered.provider_resource_id.as_deref() == Some(provider_id.as_str()),
        "partial-creation replay did not start the reserved provider identity",
    )?;
    let records = fixture
        .records_for(&restarted_driver, &request.spec)
        .await?;
    require(
        records.len() == 1 && records[0].id == provider_id,
        "partial-creation replay left duplicate provider resources",
    )?;
    fixture
        .remove_unit(&restarted, &request.spec, "recovery-partial-creation")
        .await
}

async fn create_before_ack_and_client_restart(
    fixture: &BoxRuntimeConformanceFixture,
) -> Result<()> {
    let request = fixture.cases.service(
        "recovery-create-before-ack",
        "printf 'r17-create-before-ack\\n'; exec sleep 3600",
    );
    let reservation = fixture.state.reserve_apply(&request, now_ms()).await?;
    require(
        reservation.dispatch,
        "create-before-ack reservation did not require provider work",
    )?;
    let created = fixture
        .driver
        .apply(&request.spec, &reservation.record.observation)
        .await?;
    require(
        created.state == RuntimeUnitState::Running,
        "create-before-ack provider effect did not start a Service",
    )?;
    let original_id = created
        .provider_resource_id
        .clone()
        .ok_or_else(|| super::protocol("create-before-ack returned no provider identity"))?;

    // Deliberately omit RuntimeStateStore::update_observation: this is the
    // exact provider-effect-before-durable-ack crash window.
    let restarted_driver = fixture.restarted_driver()?;
    let restarted = fixture.client_with(restarted_driver, fixture.state.clone());
    let recovered = restarted.apply(&request).await?;
    require(
        recovered.provider_resource_id.as_deref() == Some(original_id.as_str()),
        "pending replay substituted provider identity after create-before-ack",
    )?;
    let records = fixture.records_for(&fixture.driver, &request.spec).await?;
    require(
        records.len() == 1 && records[0].id == original_id,
        "create-before-ack recovery left duplicate provider resources",
    )?;
    fixture
        .remove_unit(&restarted, &request.spec, "recovery-create-before-ack")
        .await
}

async fn cancelled_task_apply_is_replayable(fixture: &BoxRuntimeConformanceFixture) -> Result<()> {
    let request = fixture.cases.task(
        "recovery-cancelled-task",
        "printf 'r17-cancelled-task\\n'; sleep 5",
        30_000,
    );
    let client = Arc::new(fixture.client_with(fixture.driver.clone(), fixture.state.clone()));
    let apply = {
        let client = client.clone();
        let request = request.clone();
        tokio::spawn(async move { client.apply(&request).await })
    };

    let provider_id = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let records = fixture.records_for(&fixture.driver, &request.spec).await?;
            require(
                records.len() <= 1,
                "cancelled Task created duplicate provider resources before cancellation",
            )?;
            if let Some(record) = records.first() {
                match local_identity(record)?.2 {
                    ManagedExecutionState::Running => break Ok(record.id.clone()),
                    ManagedExecutionState::Stopped | ManagedExecutionState::Failed => {
                        break Err(super::protocol(
                            "cancellation fixture Task became terminal before cancellation",
                        ));
                    }
                    _ => {}
                }
            }
            if apply.is_finished() {
                break Err(super::protocol(
                    "cancellation fixture apply completed before cancellation",
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| super::failure("cancellation fixture did not reach a running Sandbox"))??;

    apply.abort();
    match apply.await {
        Err(error) if error.is_cancelled() => {}
        Ok(result) => {
            return Err(super::protocol(format!(
                "cancelled Task apply completed unexpectedly: {result:?}"
            )));
        }
        Err(error) => {
            return Err(super::external("join cancelled Task apply", error));
        }
    }
    let pending = fixture
        .state
        .load_request(&request.spec.unit_id, &request.request_id)
        .await?;
    require(
        pending.state == RuntimeRequestState::Pending,
        "cancelled Task apply did not preserve a replayable pending receipt",
    )?;

    let restarted_driver = fixture.restarted_driver()?;
    let restarted = fixture.client_with(restarted_driver.clone(), fixture.state.clone());
    let recovered = restarted.apply(&request).await?;
    require(
        recovered.state == RuntimeUnitState::Succeeded
            && recovered.provider_resource_id.as_deref() == Some(provider_id.as_str()),
        "cancelled Task replay did not reattach to the original provider identity",
    )?;
    let records = fixture
        .records_for(&restarted_driver, &request.spec)
        .await?;
    require(
        records.len() == 1 && records[0].id == provider_id,
        "cancelled Task replay left duplicate provider resources",
    )?;
    fixture
        .remove_unit(&restarted, &request.spec, "recovery-cancelled-task")
        .await
}

async fn completed_client_restart(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let request = fixture.cases.service(
        "recovery-client-restart",
        "printf 'r17-client-restart\\n'; exec sleep 3600",
    );
    let first = client.apply(&request).await?;
    let restarted = fixture.client_with(fixture.driver.clone(), fixture.state.clone());
    let replay = restarted.apply(&request).await?;
    require(
        replay == first,
        "completed request changed after Runtime client restart",
    )?;
    fixture
        .remove_unit(&restarted, &request.spec, "recovery-client-restart")
        .await
}

async fn provider_restart(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let request = fixture.cases.service(
        "recovery-provider-restart",
        "printf 'r17-provider-restart\\n'; exec sleep 3600",
    );
    let running = client.apply(&request).await?;
    let provider_id = running.provider_resource_id.clone();
    let restarted_driver = fixture.restarted_driver()?;
    let restarted = fixture.client_with(restarted_driver, fixture.state.clone());
    let RuntimeInspection::Found { observation, .. } =
        restarted.inspect(&request.spec.unit_id).await?
    else {
        return Err(super::protocol(
            "provider restart lost a running Sandbox Service",
        ));
    };
    require(
        observation.state == RuntimeUnitState::Running
            && observation.provider_resource_id == provider_id,
        "provider restart changed running Sandbox identity",
    )?;
    fixture
        .remove_unit(&restarted, &request.spec, "recovery-provider-restart")
        .await
}

async fn external_deletion_and_single_replacement(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let first_request = fixture.cases.service(
        "recovery-external-loss",
        "printf 'r17-external-loss\\n'; exec sleep 3600",
    );
    let running = client.apply(&first_request).await?;
    let lost_id = running
        .provider_resource_id
        .clone()
        .ok_or_else(|| super::protocol("external-loss fixture returned no provider identity"))?;
    let record = fixture.record_for(&first_request.spec).await?;
    let home = fixture.home_dir.clone();
    let box_dir = record.box_dir.clone();
    let cleanup_id = record.id.clone();
    tokio::task::spawn_blocking(move || {
        crate::vm::reap::cleanup_recorded_sandbox_runtime_in(&home, &box_dir, &cleanup_id)
    })
    .await
    .map_err(|error| super::external("join external deletion", error))?
    .map_err(|error| super::external("delete external Sandbox resource", error))?;

    let restarted_driver = fixture.restarted_driver()?;
    let restarted = fixture.client_with(restarted_driver, fixture.state.clone());
    let RuntimeInspection::Found { observation, .. } =
        restarted.inspect(&first_request.spec.unit_id).await?
    else {
        return Err(super::protocol(
            "external deletion did not preserve durable Runtime identity",
        ));
    };
    require(
        observation.state == RuntimeUnitState::Unknown,
        "external deletion did not persist unknown before replacement",
    )?;

    let mut replacement_request = first_request.clone();
    replacement_request.request_id = fixture
        .cases
        .request_id("recovery-external-loss-replacement");
    let replacement = restarted.apply(&replacement_request).await?;
    let replacement_id = replacement
        .provider_resource_id
        .clone()
        .ok_or_else(|| super::protocol("replacement returned no provider identity"))?;
    require(
        replacement_id != lost_id,
        "confirmed provider loss reused the deleted identity",
    )?;
    let exact = restarted.apply(&replacement_request).await?;
    require(
        exact.provider_resource_id.as_deref() == Some(replacement_id.as_str()),
        "replacement replay created another provider identity",
    )?;
    let records = fixture
        .records_for(&fixture.driver, &replacement_request.spec)
        .await?;
    require(
        records.len() == 1 && records[0].id == replacement_id,
        "external-loss recovery did not converge to exactly one replacement",
    )?;
    fixture
        .remove_unit(
            &restarted,
            &replacement_request.spec,
            "recovery-external-loss",
        )
        .await
}

async fn duplicate_resource_detection(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let request = fixture.cases.service(
        "recovery-duplicate",
        "printf 'r17-duplicate\\n'; exec sleep 3600",
    );
    client.apply(&request).await?;

    let injection_operation =
        OperationId::new(format!("r17-duplicate-injection:{}", uuid::Uuid::new_v4()))
            .map_err(|error| super::invalid(error.to_string()))?;
    let reservation = fixture
        .driver
        .manager
        .create(creation_request(&request.spec)?, &injection_operation)
        .await
        .map_err(|error| super::external("reserve duplicate Sandbox", error))?;
    fixture
        .driver
        .manager
        .start(&reservation.execution_id, reservation.generation)
        .await
        .map_err(|error| super::external("start duplicate Sandbox", error))?;

    let error = client.inspect(&request.spec.unit_id).await.unwrap_err();
    require(
        matches!(error, RuntimeError::Protocol(_)),
        format!("duplicate provider resource did not fail closed: {error}"),
    )?;
    let records = fixture.records_for(&fixture.driver, &request.spec).await;
    require(
        matches!(records, Err(RuntimeError::Protocol(_))),
        "duplicate provider records were accepted as one resource",
    )?;

    let injected = fixture
        .driver
        .manager
        .managed_record(&reservation.execution_id)
        .await
        .map_err(|error| super::external("load duplicate Sandbox", error))?
        .ok_or_else(|| super::protocol("duplicate Sandbox disappeared before cleanup"))?;
    fixture
        .driver
        .retire_record(injected, &request.spec.unit_id)
        .await?;
    let remaining = fixture.driver.unit_records(&request.spec.unit_id).await?;
    require(
        remaining.len() == 1,
        "duplicate cleanup did not preserve exactly one original resource",
    )?;
    fixture
        .remove_unit(client, &request.spec, "recovery-duplicate")
        .await
}
