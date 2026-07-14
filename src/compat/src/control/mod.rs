mod credential;
mod memory;
mod model;
mod ports;
mod repository;
mod service;
mod sqlite;
mod validation;

pub use credential::{
    IssuedToken, SandboxCredentials, SecretToken, StoredToken, TokenIssuer, TokenIssuerError,
    TokenIssuerResult, TokenResolver, TokenScope,
};
pub use memory::MemorySandboxRepository;
pub use model::{
    LifecycleError, LifecycleFailure, LifecyclePolicy, LifecycleState, NewSandboxRecord,
    OnTimeoutAction, PublicSandboxState, SandboxGeneration, SandboxId, SandboxRecord,
};
pub use ports::{
    Clock, IdentityProviderError, IdentityProviderResult, ResolvedTemplate, SandboxIdentity,
    SandboxIdentityProvider, SystemClock, TemplateProvider, TemplateProviderError,
    TemplateProviderResult,
};
pub use repository::{
    CompareAndSwapResult, RepositoryError, RepositoryResult, SandboxCursor, SandboxListFilter,
    SandboxPage, SandboxRepository,
};
pub use service::{
    ConnectionDisposition, ControlService, ControlServiceDependencies, ControlServiceError,
    ControlServiceResult, CreateSandboxRequest, SandboxConnection,
};
pub use sqlite::SqliteSandboxRepository;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod service_tests;

#[cfg(test)]
pub(crate) mod test_support;
