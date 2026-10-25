//! Private backend boundary used by the lifecycle owner.
//!
//! No SDK type crosses this module. Futures are RPITIT and dispatched
//! statically through `State<B>`; there is no boxed future or trait object on
//! the lifecycle operation path. The one boxed seam is the attached process
//! transport ([`BackendProcess`]): its stdin writer, output stream, and exit
//! confirmation outlive the backend borrow and are owned by the separate I/O
//! task. A backend owns connection details and translates the shared
//! [`CommonSpec`] into its own create request.

use std::future::Future;
use std::pin::Pin;

use futures_util::Stream;
use tokio::io::AsyncWrite;

use super::engine::{
    EngineError, ExecDone, HostReport, ProcessChunk, ProcessSpec, Recovered, UploadEntry,
    WorkspaceEntry,
};

/// Backend-owned instance identity. Opaque outside the lifecycle owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstanceId(String);

impl InstanceId {
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Operations a backend can actually perform.
///
/// Creation requires guest file transport only. Pause/resume, workspace
/// snapshots, and interactive processes are negotiated when those operations
/// are requested, so a backend can create and run without them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Capabilities {
    pub pause_resume: bool,
    pub file_transport: bool,
    /// Enumerates workspace paths and reads their payloads through the guest.
    pub workspace_snapshot: bool,
    /// Attaches one interactive process to the instance.
    pub spawn_process: bool,
}

/// Backend-neutral values needed for one create request.
///
/// Backend-specific image, runtime, endpoint, and owner label stay inside the
/// backend implementation. All values were validated by `SandboxConfig`.
#[derive(Clone)]
pub(crate) struct CommonSpec {
    pub memory: i64,
    pub nano_cpus: i64,
    pub pids: i64,
    pub workspace_path: String,
    pub workspace_bytes: u64,
    pub guest_uid: u32,
    pub guest_gid: u32,
}

/// Preparation failed either because the image is absent locally or for another
/// engine reason. The image is explicit so the lifecycle owner can report it
/// without reaching into backend configuration.
#[derive(Debug)]
pub(crate) enum PrepareError {
    ImageUnavailable { image: String },
    Engine(EngineError),
}

/// One attached interactive process, backend-neutral.
///
/// The backend owns the transport; lifecycle owns cancellation and cleanup.
/// `exit` confirms the guest exit status only after `output` has ended, so it is
/// resolved inside the lifecycle I/O task instead of borrowing the backend. The
/// lifecycle worker needs no clonable or shared backend to observe an exit.
pub(crate) struct BackendProcess {
    /// Attached stdin. A dropped writer closes the transport, never the guest.
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
    /// Ordered stdout/stderr chunks until the process detaches.
    pub output: Pin<Box<dyn Stream<Item = Result<ProcessChunk, EngineError>> + Send>>,
    /// Confirmed exit status, awaited once `output` has ended.
    pub exit: Pin<Box<dyn Future<Output = Result<i64, EngineError>> + Send>>,
}

/// Deployment adapter with static dispatch.
pub(crate) trait Backend: Send + Sync + 'static {
    /// Stable label for telemetry. Never derived from a caller-supplied value.
    fn kind(&self) -> &'static str;

    /// Operations this backend can actually perform.
    fn capabilities(&self) -> Capabilities;

    /// Verifies the backend environment and that the requested image exists
    /// locally. Must not allocate an instance or pull anything.
    fn prepare(&self) -> impl Future<Output = Result<(), PrepareError>> + Send;

    /// Creates one owned instance. Partial-failure ownership is reported through
    /// [`EngineError::create_pending`] and [`EngineError::create_id`].
    fn create_owned(
        &self,
        name: &str,
        spec: &CommonSpec,
        nonce: &str,
    ) -> impl Future<Output = Result<InstanceId, EngineError>> + Send;

    /// Force-removes only an instance carrying this exact owner label.
    ///
    /// `retained_id` is used for a forced-removal attempt when a transient
    /// inspection failure prevents confirming the name. The outcome explicitly
    /// distinguishes confirmed removal/absence from still-present and unknown
    /// ownership; an unconfirmed name is never removed directly.
    fn recover_owned(
        &self,
        name: &str,
        nonce: &str,
        retained_id: Option<&InstanceId>,
    ) -> impl Future<Output = Recovered> + Send;

    fn start(&self, id: &InstanceId) -> impl Future<Output = Result<(), EngineError>> + Send;

    fn pause(&self, id: &InstanceId) -> impl Future<Output = Result<(), EngineError>> + Send;

    fn resume(&self, id: &InstanceId) -> impl Future<Output = Result<(), EngineError>> + Send;

    /// Transfers one staging entry. A file source owns an already-opened host
    /// handle and a declared length; the backend must stream it in bounded
    /// chunks instead of materializing the payload. Callers invoke this once
    /// per planned entry so no aggregate payload exists in memory.
    fn upload(
        &self,
        id: &InstanceId,
        user: &str,
        workspace: &str,
        entry: UploadEntry,
        max_bytes: u64,
    ) -> impl Future<Output = Result<(), EngineError>> + Send;

    fn download_file(
        &self,
        id: &InstanceId,
        user: &str,
        workspace: &str,
        relative: &str,
        max_bytes: usize,
    ) -> impl Future<Output = Result<Vec<u8>, EngineError>> + Send;

    fn remove_file(
        &self,
        id: &InstanceId,
        user: &str,
        workspace: &str,
        relative: &str,
    ) -> impl Future<Output = Result<(), EngineError>> + Send;

    fn exec(
        &self,
        id: &InstanceId,
        user: &str,
        workdir: &str,
        argv: &[String],
        output_limit: usize,
    ) -> impl Future<Output = Result<ExecDone, EngineError>> + Send;

    /// Attaches one interactive process to the owned instance.
    ///
    /// The request is backend-neutral; instance identity, guest user, and
    /// workspace are supplied by the lifecycle owner. A started process is
    /// owned until its transport ends; closing the transport never claims the
    /// guest stopped, so only lifecycle removes the instance.
    fn spawn_process(
        &self,
        id: &InstanceId,
        spec: ProcessSpec<'_>,
    ) -> impl Future<Output = Result<BackendProcess, EngineError>> + Send;

    /// Inspected effective configuration, with absent fields left absent.
    fn host_report(
        &self,
        id: &InstanceId,
    ) -> impl Future<Output = Result<HostReport, EngineError>> + Send;

    /// Enumerates only the configured workspace, sorted by path, without
    /// following links and without Docker's archive API.
    ///
    /// `max_bytes` bounds the transport encoding of the listing and
    /// `max_entries` bounds the number of entries; both are enforced by the
    /// backend before returning. Implementations reject symlinks, non-regular
    /// nodes, hardlinked files, non-UTF-8 names, and non-portable relative
    /// names instead of skipping them.
    fn list_workspace(
        &self,
        id: &InstanceId,
        user: &str,
        workspace: &str,
        max_bytes: u64,
        max_entries: usize,
    ) -> impl Future<Output = Result<Vec<WorkspaceEntry>, EngineError>> + Send;
}
