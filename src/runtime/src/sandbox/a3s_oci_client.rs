//! Blocking lifecycle adapter around the async A3S OCI SDK.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Duration;

use a3s_box_core::error::{BoxError, Result};
use a3s_oci_sdk::{
    ContainerOperationRequest, ContainerRecord, ContainerStats, CreateRequest, DeleteRequest,
    ExitStatus, KillRequest, LocalIpcEndpoint, RuntimeClient, RuntimeInfo, StartRequest,
    StateRequest, StatsRequest, UpdateRequest, WaitRequest,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

type SdkResult<T> = std::result::Result<T, a3s_oci_sdk::Error>;

enum ClientRequest {
    Features(SyncSender<SdkResult<RuntimeInfo>>),
    Create(Box<CreateRequest>, SyncSender<SdkResult<ContainerRecord>>),
    Start(StartRequest, SyncSender<SdkResult<ContainerRecord>>),
    State(StateRequest, SyncSender<SdkResult<ContainerRecord>>),
    Kill(KillRequest, SyncSender<SdkResult<ContainerRecord>>),
    Delete(DeleteRequest, SyncSender<SdkResult<()>>),
    Pause(
        ContainerOperationRequest,
        SyncSender<SdkResult<ContainerRecord>>,
    ),
    Resume(
        ContainerOperationRequest,
        SyncSender<SdkResult<ContainerRecord>>,
    ),
    Update(UpdateRequest, SyncSender<SdkResult<ContainerRecord>>),
    Wait(WaitRequest, SyncSender<SdkResult<ExitStatus>>),
    Stats(StatsRequest, SyncSender<SdkResult<ContainerStats>>),
    Close(SyncSender<()>),
}

impl ClientRequest {
    async fn execute(self, client: &RuntimeClient) -> bool {
        match self {
            Self::Features(reply) => send_reply(reply, client.features().await),
            Self::Create(request, reply) => send_reply(reply, client.create(*request).await),
            Self::Start(request, reply) => send_reply(reply, client.start(request).await),
            Self::State(request, reply) => send_reply(reply, client.state(request).await),
            Self::Kill(request, reply) => send_reply(reply, client.kill(request).await),
            Self::Delete(request, reply) => send_reply(reply, client.delete(request).await),
            Self::Pause(request, reply) => send_reply(reply, client.pause(request).await),
            Self::Resume(request, reply) => send_reply(reply, client.resume(request).await),
            Self::Update(request, reply) => send_reply(reply, client.update(request).await),
            Self::Wait(request, reply) => send_reply(reply, client.wait(request).await),
            Self::Stats(request, reply) => send_reply(reply, client.stats(request).await),
            Self::Close(reply) => {
                let _ = reply.send(());
                return false;
            }
        }
        true
    }
}

fn send_reply<T>(reply: SyncSender<SdkResult<T>>, result: SdkResult<T>) {
    let _ = reply.send(result);
}

/// Cloneable synchronous facade backed by one dedicated Tokio worker.
///
/// [`a3s_box_core::vmm::VmHandler`] is synchronous, while A3S OCI deliberately
/// exposes only an async SDK. Keeping the negotiated connection on a dedicated
/// thread avoids nested-runtime blocking and retains one ordered SDK session
/// for the exact Sandbox generation.
#[derive(Clone)]
pub(crate) struct A3sOciClient {
    requests: SyncSender<ClientRequest>,
}

impl A3sOciClient {
    pub(crate) async fn connect(socket_path: PathBuf) -> Result<Self> {
        tokio::task::spawn_blocking(move || Self::connect_blocking(socket_path))
            .await
            .map_err(|error| BoxError::BoxBootError {
                message: format!("A3S OCI SDK connection task failed: {error}"),
                hint: None,
            })?
    }

    pub(crate) fn connect_blocking(socket_path: PathBuf) -> Result<Self> {
        let (requests, receiver) = mpsc::sync_channel(32);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("a3s-box-oci-sdk".to_string())
            .spawn(move || run_client_worker(socket_path, receiver, ready_tx))
            .map_err(BoxError::IoError)?;

        match ready_rx.recv_timeout(CONNECT_TIMEOUT) {
            Ok(Ok(())) => Ok(Self { requests }),
            Ok(Err(error)) => Err(sdk_error("connect", error)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(BoxError::BoxBootError {
                message: "Timed out connecting to the A3S OCI runtime owner".to_string(),
                hint: None,
            }),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(BoxError::BoxBootError {
                message: "A3S OCI SDK worker exited before connection readiness".to_string(),
                hint: None,
            }),
        }
    }

    pub(crate) fn features(&self) -> Result<RuntimeInfo> {
        self.call(ClientRequest::Features)
    }

    pub(crate) fn create(&self, request: CreateRequest) -> Result<ContainerRecord> {
        self.call(|reply| ClientRequest::Create(Box::new(request), reply))
    }

    pub(crate) fn start(&self, request: StartRequest) -> Result<ContainerRecord> {
        self.call(|reply| ClientRequest::Start(request, reply))
    }

    pub(crate) fn state_optional(&self, request: StateRequest) -> Result<Option<ContainerRecord>> {
        match self.call_sdk(|reply| ClientRequest::State(request, reply)) {
            Ok(record) => Ok(Some(record)),
            Err(error) if error.code == a3s_oci_sdk::ErrorCode::NotFound => Ok(None),
            Err(error) => Err(sdk_error("state", error)),
        }
    }

    pub(crate) fn kill(&self, request: KillRequest) -> Result<ContainerRecord> {
        self.call(|reply| ClientRequest::Kill(request, reply))
    }

    pub(crate) fn delete_if_present(&self, request: DeleteRequest) -> Result<()> {
        match self.call_sdk(|reply| ClientRequest::Delete(request, reply)) {
            Ok(()) => Ok(()),
            Err(error) if error.code == a3s_oci_sdk::ErrorCode::NotFound => Ok(()),
            Err(error) => Err(sdk_error("delete", error)),
        }
    }

    pub(crate) fn pause(&self, request: ContainerOperationRequest) -> Result<ContainerRecord> {
        self.call(|reply| ClientRequest::Pause(request, reply))
    }

    pub(crate) fn resume(&self, request: ContainerOperationRequest) -> Result<ContainerRecord> {
        self.call(|reply| ClientRequest::Resume(request, reply))
    }

    pub(crate) fn update(&self, request: UpdateRequest) -> Result<ContainerRecord> {
        self.call(|reply| ClientRequest::Update(request, reply))
    }

    pub(crate) fn wait(&self, request: WaitRequest) -> Result<ExitStatus> {
        self.call(|reply| ClientRequest::Wait(request, reply))
    }

    /// Poll an exact container generation without turning a live process into
    /// an error. A zero-timeout A3S OCI wait performs one authoritative child
    /// poll and returns `DeadlineExceeded` only while no exit status exists.
    pub(crate) fn try_wait(&self, request: WaitRequest) -> Result<Option<ExitStatus>> {
        match self.call_sdk(|reply| ClientRequest::Wait(request, reply)) {
            Ok(status) => Ok(Some(status)),
            Err(error) if error.code == a3s_oci_sdk::ErrorCode::DeadlineExceeded => Ok(None),
            Err(error) => Err(sdk_error("wait", error)),
        }
    }

    pub(crate) fn stats(&self, request: StatsRequest) -> Result<ContainerStats> {
        self.call(|reply| ClientRequest::Stats(request, reply))
    }

    pub(crate) fn close(&self) {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self.requests.send(ClientRequest::Close(reply_tx)).is_ok() {
            let _ = reply_rx.recv_timeout(Duration::from_secs(1));
        }
    }

    fn call<T>(
        &self,
        request: impl FnOnce(SyncSender<SdkResult<T>>) -> ClientRequest,
    ) -> Result<T> {
        self.call_sdk(request)
            .map_err(|error| sdk_error("lifecycle", error))
    }

    fn call_sdk<T>(
        &self,
        request: impl FnOnce(SyncSender<SdkResult<T>>) -> ClientRequest,
    ) -> SdkResult<T> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.requests.send(request(reply_tx)).map_err(|_| {
            a3s_oci_sdk::Error::new(
                a3s_oci_sdk::ErrorCode::Unavailable,
                "A3S OCI SDK worker is unavailable",
            )
        })?;
        reply_rx.recv().map_err(|_| {
            a3s_oci_sdk::Error::new(
                a3s_oci_sdk::ErrorCode::Unavailable,
                "A3S OCI SDK worker dropped its reply",
            )
        })?
    }
}

fn run_client_worker(
    socket_path: PathBuf,
    receiver: Receiver<ClientRequest>,
    ready: SyncSender<SdkResult<()>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(a3s_oci_sdk::Error::new(
                a3s_oci_sdk::ErrorCode::Internal,
                format!("failed to create A3S OCI SDK runtime: {error}"),
            )));
            return;
        }
    };
    let endpoint = match LocalIpcEndpoint::unix_socket(socket_path) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let client = match runtime.block_on(RuntimeClient::connect(&endpoint)) {
        Ok(client) => client,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }
    while let Ok(request) = receiver.recv() {
        if !runtime.block_on(request.execute(&client)) {
            break;
        }
    }
}

fn sdk_error(operation: &str, error: a3s_oci_sdk::Error) -> BoxError {
    BoxError::ExecError(format!("A3S OCI {operation} failed: {error}"))
}
