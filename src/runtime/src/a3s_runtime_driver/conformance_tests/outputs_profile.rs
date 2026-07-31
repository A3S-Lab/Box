use std::collections::BTreeMap;

use a3s_runtime::contract::{RuntimeOutputSpec, RuntimeUnitState};
use a3s_runtime::RuntimeClient;

use super::fixture::{output_digest, BoxRuntimeConformanceFixture};
use super::{require, Result};

const OUTPUT_NAME: &str = "result";
const OUTPUT_PATH: &str = "/outputs/result";
const OUTPUT_FILE: &str = "result.txt";
const OUTPUT_PAYLOAD: &[u8] = b"r17-digest-bound-output";
const OUTPUT_MEDIA_TYPE: &str = "application/vnd.a3s.directory.v1+tar";
const OUTPUT_MAX_BYTES: u64 = 64;

pub(super) async fn run(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let mut request = fixture.cases.task(
        "outputs-exact-bounded",
        "printf %s r17-digest-bound-output > /outputs/result/result.txt",
        10_000,
    );
    request.spec.outputs = vec![RuntimeOutputSpec {
        name: OUTPUT_NAME.into(),
        path: OUTPUT_PATH.into(),
        media_type: OUTPUT_MEDIA_TYPE.into(),
        max_bytes: OUTPUT_MAX_BYTES,
    }];
    let spec_digest = request.spec.digest().map_err(super::invalid)?;
    let cleanup_before = fixture
        .artifact_cleanup_calls()
        .iter()
        .filter(|digest| *digest == &spec_digest)
        .count();

    let observation = client.apply(&request).await?;
    require(
        observation.state == RuntimeUnitState::Succeeded,
        "Task-output fixture did not reach succeeded",
    )?;
    require(
        observation.outputs.len() == 1,
        "Task-output fixture did not publish exactly one requested output",
    )?;

    let expected_files = BTreeMap::from([(OUTPUT_FILE.into(), OUTPUT_PAYLOAD.to_vec())]);
    let expected_digest = output_digest(&expected_files);
    let output = &observation.outputs[0];
    require(
        output.name == OUTPUT_NAME
            && output.artifact.media_type == OUTPUT_MEDIA_TYPE
            && output.artifact.digest == expected_digest
            && output.artifact.uri.ends_with(&expected_digest)
            && output.size_bytes == OUTPUT_PAYLOAD.len() as u64
            && output.size_bytes <= OUTPUT_MAX_BYTES,
        "Task-output Artifact was not exact, bounded, and digest-bound",
    )?;

    let captures = fixture
        .output_captures()
        .into_iter()
        .filter(|capture| capture.spec_digest == spec_digest)
        .collect::<Vec<_>>();
    require(
        !captures.is_empty()
            && captures.iter().all(|capture| {
                capture.name == OUTPUT_NAME
                    && capture.files == expected_files
                    && capture.artifact == *output
            }),
        "Task-output publication did not preserve the captured bytes and identity",
    )?;

    let replay = client.apply(&request).await?;
    require(
        replay == observation,
        "exact Task replay changed its published output identity",
    )?;
    let record = fixture.record_for(&request.spec).await?;
    require(
        record.volume_names.len() == 1,
        "Task-output fixture did not use exactly one Box Volume",
    )?;
    let volume_name = record.volume_names[0].clone();

    fixture
        .remove_unit(client, &request.spec, "outputs-exact-bounded")
        .await?;
    let cleanup_after = fixture
        .artifact_cleanup_calls()
        .iter()
        .filter(|digest| *digest == &spec_digest)
        .count();
    require(
        cleanup_after == cleanup_before + 1,
        "Task-output removal did not release its caller-owned Artifact state exactly once",
    )?;
    let store = crate::VolumeStore::new(
        fixture.home_dir.join("volumes.json"),
        fixture.home_dir.join("volumes"),
    );
    require(
        store
            .get(&volume_name)
            .map_err(|error| super::external("load removed Task-output Volume", error))?
            .is_none(),
        "Task-output removal retained its Box staging Volume",
    )
}
