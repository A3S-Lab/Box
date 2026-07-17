use std::collections::BTreeMap;

use a3s_runtime::contract::{
    ArtifactRef, IsolationLevel, NetworkMode, ResourceLimits, RestartPolicy,
    RuntimeActionRequest, RuntimeApplyRequest, RuntimeExecRequest, RuntimeLogQuery,
    RuntimeLogStream, RuntimeNetworkSpec, RuntimeProcessSpec, RuntimeUnitClass, RuntimeUnitSpec,
};
use a3s_runtime::{RuntimeBaseConformanceCase, RuntimeConformanceCase};

use super::super::{DOCKER_IMAGE_MANIFEST, OCI_IMAGE_MANIFEST};
use super::{require, Result};

const DEFAULT_CPU_MILLIS: u64 = 500;
const DEFAULT_MEMORY_BYTES: u64 = 128 * 1024 * 1024;
const DEFAULT_PIDS: u32 = 64;
const DEFAULT_TASK_TIMEOUT_MS: u64 = 10_000;

#[derive(Debug, Clone)]
pub(super) struct CaseFactory {
    prefix: String,
    artifact: ArtifactRef,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ResourceShape {
    pub cpu_millis: u64,
    pub memory_bytes: u64,
    pub pids: u32,
    pub execution_timeout_ms: Option<u64>,
}

impl ResourceShape {
    pub const fn service() -> Self {
        Self {
            cpu_millis: DEFAULT_CPU_MILLIS,
            memory_bytes: DEFAULT_MEMORY_BYTES,
            pids: DEFAULT_PIDS,
            execution_timeout_ms: None,
        }
    }

    pub const fn task(timeout_ms: u64) -> Self {
        Self {
            execution_timeout_ms: Some(timeout_ms),
            ..Self::service()
        }
    }
}

impl CaseFactory {
    pub(super) fn from_environment(prefix: String) -> Result<Self> {
        let image = std::env::var("A3S_BOX_RUNTIME_CONFORMANCE_IMAGE").map_err(|_| {
            super::failure(
                "A3S_BOX_RUNTIME_CONFORMANCE_IMAGE must be a digest-pinned image reference",
            )
        })?;
        let (repository, digest_hex) = image.rsplit_once("@sha256:").ok_or_else(|| {
            super::failure(
                "A3S_BOX_RUNTIME_CONFORMANCE_IMAGE must end in @sha256:<64 lowercase hex>",
            )
        })?;
        require(
            !repository.is_empty()
                && digest_hex.len() == 64
                && digest_hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "A3S_BOX_RUNTIME_CONFORMANCE_IMAGE has an invalid sha256 digest",
        )?;
        require(
            !repository.contains("//")
                && !repository.contains('@')
                && !repository.contains('?')
                && !repository.contains('#'),
            "A3S_BOX_RUNTIME_CONFORMANCE_IMAGE is not canonical",
        )?;
        let media_type = std::env::var("A3S_BOX_RUNTIME_CONFORMANCE_MEDIA_TYPE")
            .unwrap_or_else(|_| DOCKER_IMAGE_MANIFEST.to_string());
        require(
            matches!(media_type.as_str(), OCI_IMAGE_MANIFEST | DOCKER_IMAGE_MANIFEST),
            "A3S_BOX_RUNTIME_CONFORMANCE_MEDIA_TYPE is not a supported image manifest",
        )?;
        let digest = format!("sha256:{digest_hex}");
        let artifact = ArtifactRef {
            uri: format!("oci://{repository}@{digest}"),
            digest,
            media_type,
        };
        artifact.validate().map_err(super::invalid)?;
        Ok(Self { prefix, artifact })
    }

    pub(super) fn base_case(&self) -> RuntimeBaseConformanceCase {
        let task_apply = self.task(
            "base-task-success",
            "printf 'r17-base-task-success\\n'",
            DEFAULT_TASK_TIMEOUT_MS,
        );
        let service_apply = self.service(
            "base-service",
            "printf 'r17-base-service-out\\n'; printf 'r17-base-service-err\\n' >&2; exec sleep 3600",
        );
        let task_failure_apply = self.task(
            "base-task-failure",
            "printf 'r17-base-task-failure\\n' >&2; exit 17",
            DEFAULT_TASK_TIMEOUT_MS,
        );
        let task_timeout_apply = self.task(
            "base-task-timeout",
            "exec sleep 3600",
            500,
        );
        let generation_apply = self.service(
            "base-generation",
            "printf 'r17-base-generation\\n'; exec sleep 3600",
        );
        let mut generation_conflict_apply = generation_apply.clone();
        generation_conflict_apply.request_id = self.request_id("base-generation-conflict");
        generation_conflict_apply
            .spec
            .process
            .environment
            .insert("R17_CONFLICT".into(), "true".into());

        RuntimeBaseConformanceCase {
            lifecycle: RuntimeConformanceCase {
                task_remove: self.action("base-task-remove", &task_apply.spec),
                service_stop: self.action("base-service-stop", &service_apply.spec),
                service_remove: self.action("base-service-remove", &service_apply.spec),
                task_apply,
                service_apply,
            },
            task_failure_remove: self.action(
                "base-task-failure-remove",
                &task_failure_apply.spec,
            ),
            task_timeout_remove: self.action(
                "base-task-timeout-remove",
                &task_timeout_apply.spec,
            ),
            generation_remove: self.action(
                "base-generation-remove",
                &generation_apply.spec,
            ),
            task_failure_apply,
            task_timeout_apply,
            generation_apply,
            generation_conflict_apply,
        }
    }

    pub(super) fn task(
        &self,
        label: &str,
        script: &str,
        timeout_ms: u64,
    ) -> RuntimeApplyRequest {
        self.apply(
            label,
            RuntimeUnitClass::Task,
            script,
            ResourceShape::task(timeout_ms),
            RestartPolicy::Never,
        )
    }

    pub(super) fn service(&self, label: &str, script: &str) -> RuntimeApplyRequest {
        self.apply(
            label,
            RuntimeUnitClass::Service,
            script,
            ResourceShape::service(),
            RestartPolicy::Never,
        )
    }

    pub(super) fn apply(
        &self,
        label: &str,
        class: RuntimeUnitClass,
        script: &str,
        resources: ResourceShape,
        restart: RestartPolicy,
    ) -> RuntimeApplyRequest {
        RuntimeApplyRequest {
            schema: RuntimeApplyRequest::SCHEMA.into(),
            request_id: self.request_id(&format!("{label}-apply")),
            deadline_at_ms: None,
            spec: RuntimeUnitSpec {
                schema: RuntimeUnitSpec::SCHEMA.into(),
                unit_id: self.unit_id(label),
                generation: 1,
                class,
                artifact: self.artifact.clone(),
                process: RuntimeProcessSpec {
                    command: vec!["/bin/sh".into(), "-c".into()],
                    args: vec![script.into()],
                    working_directory: Some("/".into()),
                    environment: BTreeMap::from([("R17_CASE".into(), label.into())]),
                },
                mounts: Vec::new(),
                secrets: Vec::new(),
                network: RuntimeNetworkSpec {
                    mode: NetworkMode::None,
                    ports: Vec::new(),
                },
                resources: ResourceLimits {
                    cpu_millis: resources.cpu_millis,
                    memory_bytes: resources.memory_bytes,
                    pids: resources.pids,
                    ephemeral_storage_bytes: None,
                    execution_timeout_ms: resources.execution_timeout_ms,
                },
                isolation: IsolationLevel::Sandbox,
                health: None,
                restart,
                outputs: Vec::new(),
                semantics_profile_digest: None,
            },
        }
    }

    pub(super) fn action(&self, label: &str, spec: &RuntimeUnitSpec) -> RuntimeActionRequest {
        RuntimeActionRequest {
            schema: RuntimeActionRequest::SCHEMA.into(),
            request_id: self.request_id(label),
            unit_id: spec.unit_id.clone(),
            generation: spec.generation,
            deadline_at_ms: None,
        }
    }

    pub(super) fn exec(
        &self,
        label: &str,
        spec: &RuntimeUnitSpec,
        command: Vec<String>,
        timeout_ms: u64,
    ) -> RuntimeExecRequest {
        RuntimeExecRequest {
            schema: RuntimeExecRequest::SCHEMA.into(),
            request_id: self.request_id(label),
            unit_id: spec.unit_id.clone(),
            generation: spec.generation,
            command,
            timeout_ms,
            deadline_at_ms: None,
        }
    }

    pub(super) fn logs(
        &self,
        spec: &RuntimeUnitSpec,
        cursor: Option<String>,
        limit: u32,
        stream: Option<RuntimeLogStream>,
    ) -> RuntimeLogQuery {
        RuntimeLogQuery {
            schema: RuntimeLogQuery::SCHEMA.into(),
            unit_id: spec.unit_id.clone(),
            generation: spec.generation,
            cursor,
            limit,
            stream,
        }
    }

    pub(super) fn request_id(&self, label: &str) -> String {
        format!("{}-{label}", self.prefix)
    }

    pub(super) fn unit_id(&self, label: &str) -> String {
        format!("{}-{label}", self.prefix)
    }
}
