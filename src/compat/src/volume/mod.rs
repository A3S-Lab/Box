mod filesystem;
mod memory;
mod model;
mod mount;
mod repository;
mod runtime;
mod service;
mod sqlite;

pub use filesystem::{
    current_volume_id_mapper, IdentityVolumeIdMapper, PendingVolumeWrite, SandboxVolumeIdMapper,
    VolumeContentError, VolumeContentResult, VolumeEntry, VolumeEntryType, VolumeFilesystem,
    VolumeIdMapper, VolumeMetadataUpdate, MAX_DIRECTORY_DEPTH,
};
pub use memory::MemoryVolumeRepository;
pub use model::{valid_volume_name, VolumeId, VolumeModelError, VolumeRecord, VolumeState};
pub use mount::{validate_mounts, ResolvedVolumeMount, VolumeMount, VolumeMountResolver};
pub use repository::{
    VolumeReplaceResult, VolumeRepository, VolumeRepositoryError, VolumeRepositoryResult,
};
pub use runtime::{
    A3sRuntimeVolumeStore, RuntimeVolume, RuntimeVolumeError, RuntimeVolumeRemoveResult,
    RuntimeVolumeResult, RuntimeVolumeStore,
};
pub use service::{
    AuthorizedVolume, VolumeConnection, VolumeReconciliationReport, VolumeService,
    VolumeServiceDependencies, VolumeServiceError, VolumeServiceResult,
};
pub use sqlite::SqliteVolumeRepository;

#[cfg(test)]
pub(crate) mod tests;
