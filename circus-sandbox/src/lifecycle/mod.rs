//! Lifecycle owner for one sandbox instance.

mod cleanup;
mod environment;
mod input;
mod model;
mod operations;
mod process;
mod recovery;
mod snapshot;
mod staging;

pub use cleanup::CleanupReceipt;
pub use input::{CommandError, StagingError};
pub use model::{
    CleanupFailure, CleanupReport, CommandOutput, CommandRequest, EnforcementReport, OutputPhase,
    Ownership, RecoveryId, RecoveryIdError, SandboxError, SandboxOperation, SandboxPhase,
    SandboxStatus,
};
pub use process::{
    ProcessError, ProcessExit, ProcessOutput, ProcessRequest, ProcessSession, ProcessTermination,
};

use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[cfg(all(unix, feature = "containerd"))]
use crate::infrastructure::ContainerdConfigError;
#[cfg(feature = "kubernetes")]
use crate::infrastructure::KubernetesConfigError;
use crate::infrastructure::{Backend, CommonSpec, DockerConfigError, InstanceId, PrepareError};
use crate::observability::{OperationKind, OperationOutcome, SandboxMetrics, events};
use crate::runtime::{BackendConfig, BackendConfigError, SandboxConfig};
use crate::workspace::Workspace;

use cleanup::{cleanup_until_done, failed_creation};
use operations::{
    common_spec, destroy_report, engine_err, path_text, require_capabilities, spawn_block,
};
use recovery::{ConnectedBackend, connect_backend};

const QUEUE: usize = 8;

struct CommandGuard {
    token: CancellationToken,
    armed: bool,
}

impl Drop for CommandGuard {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

enum Op {
    Start(oneshot::Sender<Result<(), SandboxError>>),
    Pause(oneshot::Sender<Result<(), SandboxError>>),
    Resume(oneshot::Sender<Result<(), SandboxError>>),
    Destroy(oneshot::Sender<Result<(), SandboxError>>),
    Write {
        path: String,
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<(), SandboxError>>,
    },
    Read {
        path: String,
        reply: oneshot::Sender<Result<Vec<u8>, SandboxError>>,
    },
    Mkdir {
        path: String,
        reply: oneshot::Sender<Result<(), SandboxError>>,
    },
    Unlink {
        path: String,
        reply: oneshot::Sender<Result<(), SandboxError>>,
    },
    Exec {
        request: CommandRequest,
        reply: oneshot::Sender<Result<CommandOutput, SandboxError>>,
    },
    Spawn {
        request: ProcessRequest,
        reply: oneshot::Sender<Result<ProcessSession, SandboxError>>,
    },
    Status(oneshot::Sender<Result<SandboxStatus, SandboxError>>),
    CaptureSnapshot {
        budget: crate::workspace::SnapshotBudget,
        /// Public `max_archive_bytes`: the manifest transport cap.
        max_manifest_bytes: u64,
        reply: oneshot::Sender<Result<Arc<crate::workspace::CodeSnapshot>, SandboxError>>,
    },
    RestoreSnapshot {
        snapshot: Arc<crate::workspace::CodeSnapshot>,
        reply: oneshot::Sender<Result<(), SandboxError>>,
    },
}

struct Message {
    op: Op,
    cancel: CancellationToken,
}

/// One owned sandbox. Drop requests cleanup without claiming cleanup succeeded.
pub struct Sandbox {
    tx: mpsc::Sender<Message>,
    shutdown: CancellationToken,
    cleanup: CleanupReceipt,
    max_file_bytes: usize,
    recovery_id: RecoveryId,
}

impl Sandbox {
    /// Interrupts queued or running operations and observes full owned cleanup.
    pub(crate) fn revoke(&self) -> CleanupReceipt {
        self.shutdown.cancel();
        self.cleanup.clone()
    }

    /// Returns the identity used for this instance's name and owner label.
    ///
    /// Persist this value if the instance may outlive the in-process handle.
    /// [`recover_orphan`](Self::recover_orphan) accepts it later and never
    /// removes an instance until the backend confirms the matching owner label.
    #[must_use]
    pub fn recovery_id(&self) -> &RecoveryId {
        &self.recovery_id
    }

    /// Validates `backend` and `config`, prepares the backend, and creates one
    /// owned instance.
    ///
    /// Backend-neutral and backend-specific values are rejected before any
    /// instance allocation. Guest file transport is required before creating an
    /// instance; pause/resume, workspace snapshots, and interactive processes
    /// negotiate their own capability when requested. Docker and containerd
    /// verify that the image already exists locally and never pull. The
    /// Kubernetes backend loads an explicit kubeconfig, verifies API access and
    /// a server-side dry run, and leaves image pulling to the node's kubelet
    /// under `imagePullPolicy: IfNotPresent`.
    ///
    /// The owner label and instance name are fixed before the create request.
    /// Dropping this future, including while the create response is outstanding,
    /// makes the worker remove only that name after confirming the label. A
    /// single not-found during the request window is not treated as finished.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::BackendConfig`] for an invalid descriptor,
    /// [`SandboxError::Config`] for invalid neutral configuration,
    /// [`SandboxError::UnsupportedCapability`] when guest file transport is not
    /// available, [`SandboxError::BackendImageUnavailable`] when a local image
    /// is absent for a local backend, [`SandboxError::Backend`] or
    /// [`SandboxError::Workspace`] for other preparation failures, and
    /// [`SandboxError::PolicyMismatch`] when the inspected effective policy
    /// differs from the constructed policy.
    /// A mismatch always initiates destruction and is never reported as
    /// success. If creation leaves cleanup outstanding,
    /// [`SandboxError::CreationCleanupRequired`] carries the original error and
    /// a receipt for instance and staging cleanup. Its worker retries
    /// independently of the receipt or this future's lifetime.
    pub async fn create(
        backend: BackendConfig,
        config: SandboxConfig,
    ) -> Result<Self, SandboxError> {
        Self::create_with_metrics(backend, config, SandboxMetrics::disabled()).await
    }

    /// Same contract as [`create`](Self::create), using a caller-supplied
    /// [`RecoveryId`] for the instance name and owner label.
    ///
    /// The caller should persist the ID together with the backend descriptor
    /// **before** calling this method, so an orphan can be inspected after a
    /// process restart. The ID is opaque and validates at construction:
    /// [`RecoveryId::generate`] creates a random UUID v4 and
    /// [`RecoveryId::parse`] accepts only canonical values, so malformed names
    /// cannot reach a backend.
    ///
    /// Reusing an ID is not deduplicated: a second create may conflict at the
    /// backend, and it never adopts or deletes an existing instance. Pass the
    /// ID to [`recover_orphan`](Self::recover_orphan) only when the original
    /// handle is gone.
    pub async fn create_with_recovery_id(
        backend: BackendConfig,
        config: SandboxConfig,
        recovery_id: RecoveryId,
    ) -> Result<Self, SandboxError> {
        Self::create_observed(backend, config, SandboxMetrics::disabled(), recovery_id).await
    }

    /// Same contract as [`create`](Self::create), with caller-owned counters
    /// and structured tracing events.
    ///
    /// `metrics` is cloned into the lifecycle worker; clones share counters.
    /// Pass [`SandboxMetrics::disabled`] for the no-op path used by
    /// [`create`](Self::create). Creation success and failure are counted, and
    /// a failed creation whose owned cleanup is still owed is reflected in
    /// `create_cleanup_pending` until cleanup confirms completion. This method
    /// does not install a tracing subscriber and exposes no health endpoint.
    pub async fn create_with_metrics(
        backend: BackendConfig,
        config: SandboxConfig,
        metrics: SandboxMetrics,
    ) -> Result<Self, SandboxError> {
        Self::create_observed(backend, config, metrics, RecoveryId::generate()).await
    }

    async fn create_observed(
        backend: BackendConfig,
        config: SandboxConfig,
        metrics: SandboxMetrics,
        recovery_id: RecoveryId,
    ) -> Result<Self, SandboxError> {
        let backend_kind = match &backend {
            BackendConfig::Docker(_) => "docker",
            BackendConfig::Containerd(_) => "containerd",
            BackendConfig::Kubernetes(_) => "kubernetes",
        };
        let span = events::create_span(backend_kind);
        {
            let _entered = span.enter();
            events::create_started(backend_kind);
        }
        let result = Self::create_inner(backend, config, metrics.clone(), recovery_id).await;
        {
            let _entered = span.enter();
            match &result {
                Ok(_) => {
                    metrics.record_create_ok();
                    events::create_completed("ok", false, "none");
                }
                Err(err) => {
                    metrics.record_create_failed();
                    if let SandboxError::UnsupportedCapability { capability } = err {
                        events::capability_unavailable(capability);
                    }
                    let cleanup_pending =
                        matches!(err, SandboxError::CreationCleanupRequired { .. });
                    events::create_completed("error", cleanup_pending, operations::error_kind(err));
                }
            }
        }
        result
    }

    async fn create_inner(
        backend: BackendConfig,
        config: SandboxConfig,
        metrics: SandboxMetrics,
        recovery_id: RecoveryId,
    ) -> Result<Self, SandboxError> {
        let spec = common_spec(&config);
        let connected = connect_backend(backend, Some(&spec), config.lifecycle_timeout()).await?;
        match connected {
            ConnectedBackend::Docker(backend) => {
                Self::create_with_backend(backend, spec, config, metrics, recovery_id).await
            }
            #[cfg(all(unix, feature = "containerd"))]
            ConnectedBackend::Containerd(backend) => {
                Self::create_with_backend(backend, spec, config, metrics, recovery_id).await
            }
            #[cfg(feature = "kubernetes")]
            ConnectedBackend::Kubernetes(backend) => {
                Self::create_with_backend(backend, spec, config, metrics, recovery_id).await
            }
        }
    }

    async fn create_with_backend<B: Backend>(
        backend: B,
        spec: CommonSpec,
        config: SandboxConfig,
        metrics: SandboxMetrics,
        recovery_id: RecoveryId,
    ) -> Result<Self, SandboxError> {
        events::backend_selected(backend.kind());
        require_capabilities(&backend)?;
        match backend.prepare().await {
            Ok(()) => {}
            Err(PrepareError::ImageUnavailable { image }) => {
                return Err(SandboxError::BackendImageUnavailable { image });
            }
            Err(PrepareError::Engine(err)) => return Err(engine_err(err)),
        }
        let nonce = recovery_id.as_str().to_string();
        let name = format!("circus-sandbox-{nonce}");
        let max_file = config.limits().max_file_bytes();
        let workspace = spawn_block(move || Workspace::new(max_file)).await??;
        let (tx, rx) = mpsc::channel(QUEUE);
        let shutdown = CancellationToken::new();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (cleanup_tx, cleanup) = cleanup::channel(name.clone());
        let state = State {
            backend,
            spec,
            workspace: Arc::new(Mutex::new(workspace)),
            user: format!(
                "{}:{}",
                config.isolation().guest_uid(),
                config.isolation().guest_gid()
            ),
            config,
            name,
            nonce,
            id: None,
            phase: SandboxPhase::Created,
            shutdown: shutdown.clone(),
            metrics,
            create_unsettled: true,
            creation_cleanup: false,
            process: None,
            cleanup: cleanup_tx,
        };
        tokio::spawn(worker(state, rx, ready_tx));
        let mut guard = CommandGuard {
            token: shutdown.clone(),
            armed: true,
        };
        let ready = ready_rx.await.map_err(|_| SandboxError::WorkerGone {
            detail: "create worker ended before reporting ownership".to_string(),
        })?;
        guard.armed = false;
        ready?;
        Ok(Self {
            tx,
            shutdown,
            cleanup,
            max_file_bytes: max_file,
            recovery_id,
        })
    }

    /// Starts the instance after validating all staged metadata.
    ///
    /// Every staged path, per-file length, and aggregate payload bound is
    /// checked from workspace metadata before the backend start call. Payloads
    /// are then streamed one entry at a time through bounded copy buffers; this
    /// crate never materializes the whole staged tree in memory. The configured
    /// workspace byte bound is the enforced transfer payload bound, while the
    /// guest workspace tmpfs remains the enforced storage bound.
    ///
    /// [`SandboxError::Staging`] leaves the instance in `Created`, with its
    /// files and ownership retained. Remove or shrink files and retry, or
    /// explicitly [`destroy`](Self::destroy) it.
    ///
    /// Uses the configured lifecycle deadline for the whole transition. A
    /// timeout or cancellation during transfer destroys the owned instance
    /// because the engine may already have accepted the start and guest files
    /// may be partially written; the phase is never reported as `Running`.
    /// The returned error reports observed ownership.
    pub async fn start(&self) -> Result<(), SandboxError> {
        self.call(Op::Start).await?
    }

    /// Uses the configured lifecycle deadline. A timeout or cancellation
    /// destroys the owned instance because the transition may already have been
    /// applied.
    pub async fn pause(&self) -> Result<(), SandboxError> {
        self.call(Op::Pause).await?
    }

    /// Uses the configured lifecycle deadline. A timeout or cancellation
    /// destroys the owned instance because the transition may already have been
    /// applied.
    pub async fn resume(&self) -> Result<(), SandboxError> {
        self.call(Op::Resume).await?
    }

    /// Force-removes the owned instance and its staging.
    ///
    /// This operation has no caller deadline and is never cancelled by dropping
    /// the future: losing the caller cannot relinquish ownership. Repeat and
    /// concurrent calls are serialized by the worker; after confirmed absence
    /// they return `Ok` without contacting the backend again.
    ///
    /// Failure returns [`SandboxError::CleanupRequired`] with the observed
    /// [`Ownership`] (`StillPresent` or `Unknown`) and a typed
    /// [`CleanupFailure`] source. The handle retains its instance identity and
    /// staging, and calling `destroy` again retries cleanup. Success is never
    /// reported while ownership is unconfirmed.
    pub async fn destroy(&self) -> Result<(), SandboxError> {
        self.call(Op::Destroy).await?
    }

    /// Stages bytes in `Created` or uploads to the guest in `Running`.
    ///
    /// Uses the configured file deadline. A timeout or cancellation destroys
    /// the owned instance because the transport may have partially applied the
    /// write; cleanup failure is reported through the error and remains
    /// retryable via [`destroy`](Self::destroy).
    pub async fn write_file(
        &self,
        path: impl AsRef<Path>,
        contents: &[u8],
    ) -> Result<(), SandboxError> {
        let path = path_text(path.as_ref())?;
        if contents.len() > self.max_file_bytes {
            return Err(crate::workspace::WorkspaceError::LimitExceeded {
                limit: self.max_file_bytes,
                actual: contents.len(),
            }
            .into());
        }
        self.call(|reply| Op::Write {
            path,
            bytes: contents.to_vec(),
            reply,
        })
        .await?
    }

    /// Reads from host staging in `Created` or the guest workspace in `Running`.
    ///
    /// Uses the configured file deadline; a timeout or cancellation destroys
    /// the owned instance.
    pub async fn read_file(&self, path: impl AsRef<Path>) -> Result<Vec<u8>, SandboxError> {
        let path = path_text(path.as_ref())?;
        self.call(|reply| Op::Read { path, reply }).await?
    }

    /// Creates a directory in staging or the guest workspace.
    ///
    /// Uses the configured file deadline; a timeout or cancellation destroys
    /// the owned instance.
    pub async fn create_dir_all(&self, path: impl AsRef<Path>) -> Result<(), SandboxError> {
        let path = path_text(path.as_ref())?;
        self.call(|reply| Op::Mkdir { path, reply }).await?
    }

    /// Unlinks a file using the confined guest transport when running.
    ///
    /// Uses the configured file deadline; a timeout or cancellation destroys
    /// the owned instance.
    pub async fn remove_file(&self, path: impl AsRef<Path>) -> Result<(), SandboxError> {
        let path = path_text(path.as_ref())?;
        self.call(|reply| Op::Unlink { path, reply }).await?
    }

    /// Runs `request` inside the running instance.
    ///
    /// Dropping this future, including while the exec request has not returned,
    /// destroys the owned instance. A pending response is not treated as
    /// "not accepted". Timeout and observed output overflow do the same.
    ///
    /// stdout and stderr share the request's single `output_limit_bytes`
    /// budget. Exactly-at-limit output is accepted; the first byte over is
    /// [`SandboxError::OutputLimit`] naming the stream, and the worker reclaims
    /// the container and its descendants. A failed reclamation is reported
    /// through `ownership`, never rewritten as success.
    ///
    /// The effective command deadline is the smaller of `request.timeout` and
    /// the configured [`crate::runtime::IoLimits::command_timeout`] ceiling.
    ///
    /// # Errors
    ///
    /// [`SandboxError::Command`] rejects empty argv, zero timeout, and zero
    /// output bound before queueing, without changing phase or ownership.
    /// A request above the configured output bound returns
    /// [`SandboxError::OutputLimit`] with phase `NotStarted` and `StillPresent`
    /// without executing.
    pub async fn exec(&self, request: CommandRequest) -> Result<CommandOutput, SandboxError> {
        if request.argv.is_empty() {
            return Err(CommandError::EmptyArgv.into());
        }
        if request.timeout.is_zero() {
            return Err(CommandError::InvalidTimeout {
                timeout: request.timeout,
            }
            .into());
        }
        if request.output_limit_bytes == 0 {
            return Err(CommandError::InvalidOutputLimit {
                output_limit_bytes: request.output_limit_bytes,
            }
            .into());
        }
        self.call(|reply| Op::Exec { request, reply }).await?
    }

    /// Starts one interactive process without occupying the lifecycle queue.
    ///
    /// Files, status, pause/resume, and destroy remain usable while it runs.
    /// Another interactive process or one-shot [`exec`](Self::exec) returns
    /// [`SandboxError::ProcessActive`] until lifecycle records completion.
    ///
    /// `ProcessRequest::validate` rejects malformed requests before queueing:
    /// no process is started, no deadline applies, and phase and ownership are
    /// unchanged. A request whose per-stream output limit exceeds the configured
    /// [`crate::runtime::IoLimits::output_limit_bytes`] is rejected the same way.
    ///
    /// Uses the configured file deadline for attachment. Dropping this future
    /// after acceptance, a deadline, or cancellation destroys the owned
    /// instance because the exec may already have been accepted; the error
    /// reports observed ownership. A normal exit preserves container and
    /// workspace; process timeout, output overflow, transport failure,
    /// session drop, or [`ProcessSession::cancel`] request whole-container
    /// cleanup and report it through [`ProcessExit::Terminated`].
    pub async fn spawn_process(
        &self,
        request: ProcessRequest,
    ) -> Result<ProcessSession, SandboxError> {
        request.validate()?;
        self.call(|reply| Op::Spawn { request, reply }).await?
    }

    /// Returns the phase, retained instance identity, and inspected enforcement
    /// evidence. Requested limits are never reported as enforced.
    ///
    /// Status uses the configured lifecycle/status deadline but is read-only:
    /// a timeout or cancellation returns [`SandboxError::TimedOut`] or
    /// [`SandboxError::Cancelled`] with [`SandboxOperation::Status`] and leaves
    /// the phase, identity, and ownership intact. Retry `status`; nothing is
    /// destroyed. Other timed-out operations destroy the instance instead.
    pub async fn status(&self) -> Result<SandboxStatus, SandboxError> {
        self.call(Op::Status).await?
    }

    async fn call<T>(&self, op: impl FnOnce(oneshot::Sender<T>) -> Op) -> Result<T, SandboxError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let cancel = CancellationToken::new();
        let mut guard = CommandGuard {
            token: cancel.clone(),
            armed: true,
        };
        self.tx
            .send(Message {
                op: op(reply_tx),
                cancel,
            })
            .await
            .map_err(|_| SandboxError::WorkerGone {
                detail: "worker stopped accepting operations".to_string(),
            })?;
        let result = reply_rx.await.map_err(|_| SandboxError::WorkerGone {
            detail: "worker dropped the operation reply".to_string(),
        });
        guard.armed = false;
        result
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandbox")
            .field("recovery_id", &self.recovery_id)
            .field("max_file_bytes", &self.max_file_bytes)
            .finish_non_exhaustive()
    }
}

fn map_docker_config(err: DockerConfigError) -> BackendConfigError {
    match err {
        DockerConfigError::UnsupportedEndpoint { value } => {
            BackendConfigError::UnsupportedEndpoint { value }
        }
        DockerConfigError::UnsupportedRuntime { name } => {
            BackendConfigError::UnsupportedRuntime { name }
        }
        DockerConfigError::ImageRequired => BackendConfigError::ImageRequired,
        DockerConfigError::SupervisorNeedsPid { pids } => {
            BackendConfigError::SupervisorNeedsPid { pids }
        }
    }
}

#[cfg(all(unix, feature = "containerd"))]
fn map_containerd_config(err: ContainerdConfigError) -> BackendConfigError {
    match err {
        ContainerdConfigError::InvalidSocketPath { value } => {
            BackendConfigError::InvalidSocketPath { value }
        }
        ContainerdConfigError::InvalidNamespace { value } => {
            BackendConfigError::InvalidNamespace { value }
        }
        ContainerdConfigError::ImageRequired => BackendConfigError::ImageRequired,
        ContainerdConfigError::RuntimeUnavailable { name } => {
            BackendConfigError::RuntimeUnavailable { name }
        }
        ContainerdConfigError::UnsupportedSnapshotter { name } => {
            BackendConfigError::UnsupportedSnapshotter { name }
        }
    }
}

#[cfg(feature = "kubernetes")]
fn map_kubernetes_config(err: KubernetesConfigError) -> BackendConfigError {
    match err {
        KubernetesConfigError::InvalidKubeconfigPath { value } => {
            BackendConfigError::InvalidKubeconfigPath { value }
        }
        KubernetesConfigError::InvalidNamespace { value } => {
            BackendConfigError::InvalidKubernetesNamespace { value }
        }
        KubernetesConfigError::InvalidContextName { value } => {
            BackendConfigError::InvalidContextName { value }
        }
        KubernetesConfigError::InvalidServiceAccount { value } => {
            BackendConfigError::InvalidServiceAccount { value }
        }
        KubernetesConfigError::InvalidImageReference { value } => {
            BackendConfigError::InvalidImageReference { value }
        }
        KubernetesConfigError::ImageRequired => BackendConfigError::ImageRequired,
        KubernetesConfigError::RootUserUnsupported { uid } => {
            BackendConfigError::NonRootGuestRequired { uid }
        }
    }
}

/// Stable telemetry label for an operation result.
fn outcome_label<T>(result: &Result<T, SandboxError>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(SandboxError::Cancelled { .. }) => "cancelled",
        Err(SandboxError::TimedOut { .. }) => "timed_out",
        Err(_) => "error",
    }
}

/// Fixed metric outcome for an operation result.
fn outcome<T>(result: &Result<T, SandboxError>) -> OperationOutcome {
    match result {
        Ok(_) => OperationOutcome::Ok,
        Err(SandboxError::Cancelled { .. }) => OperationOutcome::Cancelled,
        Err(SandboxError::TimedOut { .. }) => OperationOutcome::TimedOut,
        Err(_) => OperationOutcome::Error,
    }
}

/// Whether this result retains owned cleanup.
fn cleanup_required<T>(result: &Result<T, SandboxError>) -> bool {
    matches!(result, Err(SandboxError::CleanupRequired { .. }))
        || matches!(result, Err(SandboxError::CreationCleanupRequired { .. }))
}

/// Records one operation outcome on the worker's caller-owned handle.
fn record_operation<T>(
    metrics: &SandboxMetrics,
    kind: OperationKind,
    result: &Result<T, SandboxError>,
) {
    metrics.record_operation(kind, outcome(result));
}

pub(super) struct State<B: Backend> {
    backend: B,
    spec: CommonSpec,
    workspace: Arc<Mutex<Workspace>>,
    config: SandboxConfig,
    name: String,
    nonce: String,
    user: String,
    id: Option<InstanceId>,
    phase: SandboxPhase,
    shutdown: CancellationToken,
    metrics: SandboxMetrics,
    create_unsettled: bool,
    /// Telemetry-only marker: this worker is cleaning up a failed creation.
    creation_cleanup: bool,
    /// At most one interactive process; lifecycle records its final outcome.
    process: Option<process::ActiveProcess>,
    cleanup: tokio::sync::watch::Sender<cleanup::Progress>,
}

async fn worker<B: Backend>(
    mut state: State<B>,
    mut rx: mpsc::Receiver<Message>,
    ready: oneshot::Sender<Result<(), SandboxError>>,
) {
    let created = state
        .backend
        .create_owned(&state.name, &state.spec, &state.nonce)
        .await;
    match created {
        Ok(id) => {
            state.id = Some(id);
            state.create_unsettled = false;
            if ready.send(Ok(())).is_err() {
                cleanup_until_done(&mut state).await;
                return;
            }
        }
        Err(err) => {
            if let Some(id) = err.create_id.clone() {
                state.id = Some(InstanceId::new(id));
                state.create_unsettled = false;
            }
            if err.create_pending {
                failed_creation(&mut state, ready, engine_err(err), None).await;
                return;
            }
            state.create_unsettled = false;
            let report = destroy_report(&mut state).await;
            if state.phase == SandboxPhase::Destroyed {
                let _ = ready.send(Err(engine_err(err)));
            } else {
                failed_creation(&mut state, ready, engine_err(err), Some(report)).await;
            }
            return;
        }
    }
    loop {
        let message = tokio::select! {
            biased;
            () = state.shutdown.cancelled() => None,
            result = process::completed(&mut state.process) => {
                process::finish(&mut state, result).await;
                continue;
            },
            message = rx.recv() => message,
        };
        let Some(message) = message else {
            cleanup_until_done(&mut state).await;
            return;
        };
        // A cancelled queued request must never reach a backend or workspace.
        if message.cancel.is_cancelled() {
            continue;
        }
        dispatch(&mut state, message).await;
    }
}

async fn dispatch<B: Backend>(state: &mut State<B>, message: Message) {
    let Message { op, cancel } = message;
    let shutdown = state.shutdown.clone();
    let file_timeout = state.config.file_timeout();
    let lifecycle_timeout = state.config.lifecycle_timeout();
    // Owned transitions and transports destroy the instance on cancellation or
    // timeout because the engine may already have applied the operation. Drop
    // the outstanding backend future before attempting removal.
    macro_rules! run {
        ($future:expr, $timeout:expr, $operation:expr) => {{
            let outcome = {
                let call = $future;
                tokio::pin!(call);
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => Err(true),
                    () = shutdown.cancelled() => Err(true),
                    () = tokio::time::sleep($timeout) => Err(false),
                    result = &mut call => Ok(result),
                }
            };
            match outcome {
                Ok(result) => result,
                Err(true) => {
                    operations::destroy_for_cancel(
                        state,
                        $operation,
                        "operation cancelled while acceptance was uncertain",
                    )
                    .await
                }
                Err(false) => {
                    let report = destroy_report(state).await;
                    Err(SandboxError::TimedOut {
                        instance_id: report.instance_id,
                        ownership: report.ownership,
                        operation: $operation,
                        detail: report.detail,
                    })
                }
            }
        }};
    }
    // Read-only status inspection: a deadline or cancellation never destroys the
    // instance; phase and ownership stay retryable.
    macro_rules! observe {
        ($future:expr, $timeout:expr) => {{
            let outcome = {
                let call = $future;
                tokio::pin!(call);
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => Err(true),
                    () = shutdown.cancelled() => Err(true),
                    () = tokio::time::sleep($timeout) => Err(false),
                    result = &mut call => Ok(result),
                }
            };
            match outcome {
                Ok(result) => result,
                Err(cancelled) => {
                    let instance_id = state.id.as_ref().map(|id| id.as_str().to_string());
                    let ownership = if instance_id.is_some() {
                        Ownership::StillPresent
                    } else {
                        Ownership::Unknown
                    };
                    Err(if cancelled {
                        SandboxError::Cancelled {
                            instance_id,
                            ownership,
                            operation: SandboxOperation::Status,
                            detail: "status observation cancelled; phase and ownership retained"
                                .to_string(),
                        }
                    } else {
                        SandboxError::TimedOut {
                            instance_id,
                            ownership,
                            operation: SandboxOperation::Status,
                            detail: "status deadline elapsed; phase and ownership retained"
                                .to_string(),
                        }
                    })
                }
            }
        }};
    }
    match op {
        Op::Start(reply) => {
            let result = run!(
                operations::start(state),
                lifecycle_timeout,
                SandboxOperation::Start
            );
            events::transition_completed(
                "start",
                outcome_label(&result),
                cleanup_required(&result),
            );
            record_operation(&state.metrics, OperationKind::Start, &result);
            let _ = reply.send(result);
        }
        Op::Pause(reply) => {
            let result = run!(
                operations::pause(state),
                lifecycle_timeout,
                SandboxOperation::Pause
            );
            events::transition_completed(
                "pause",
                outcome_label(&result),
                cleanup_required(&result),
            );
            record_operation(&state.metrics, OperationKind::Pause, &result);
            let _ = reply.send(result);
        }
        Op::Resume(reply) => {
            let result = run!(
                operations::resume(state),
                lifecycle_timeout,
                SandboxOperation::Resume
            );
            events::transition_completed(
                "resume",
                outcome_label(&result),
                cleanup_required(&result),
            );
            record_operation(&state.metrics, OperationKind::Resume, &result);
            let _ = reply.send(result);
        }
        // Never cancel cleanup itself: a lost caller cannot relinquish ownership.
        Op::Destroy(reply) => {
            let (_, error) = operations::destroy_attempt(state).await;
            let result = if state.phase == SandboxPhase::Destroyed {
                Ok(())
            } else {
                // `destroy_state` always reports a typed CleanupRequired here;
                // preserve it instead of synthesizing a weaker error.
                Err(error.unwrap_or_else(|| SandboxError::CleanupRequired {
                    instance_id: state
                        .id
                        .as_ref()
                        .map_or_else(|| state.name.clone(), |id| id.as_str().to_string()),
                    ownership: Ownership::Unknown,
                    detail: "destroy did not confirm owned cleanup".to_string(),
                    failure: CleanupFailure::Unknown {
                        detail: "destroy did not confirm owned cleanup".to_string(),
                    },
                }))
            };
            record_operation(&state.metrics, OperationKind::Destroy, &result);
            let _ = reply.send(result);
        }
        Op::Write { path, bytes, reply } => {
            let result = run!(
                operations::write_file(state, &path, bytes),
                file_timeout,
                SandboxOperation::WriteFile
            );
            record_operation(&state.metrics, OperationKind::WriteFile, &result);
            let _ = reply.send(result);
        }
        Op::Read { path, reply } => {
            let result = run!(
                operations::read_file(state, &path),
                file_timeout,
                SandboxOperation::ReadFile
            );
            record_operation(&state.metrics, OperationKind::ReadFile, &result);
            let _ = reply.send(result);
        }
        Op::Mkdir { path, reply } => {
            let result = run!(
                operations::mkdir(state, &path),
                file_timeout,
                SandboxOperation::CreateDirAll
            );
            record_operation(&state.metrics, OperationKind::CreateDirAll, &result);
            let _ = reply.send(result);
        }
        Op::Unlink { path, reply } => {
            let result = run!(
                operations::unlink(state, &path),
                file_timeout,
                SandboxOperation::RemoveFile
            );
            record_operation(&state.metrics, OperationKind::RemoveFile, &result);
            let _ = reply.send(result);
        }
        Op::Exec { request, reply } => {
            // The configured command timeout is an operator ceiling; a request
            // may ask for a shorter deadline but never a longer one.
            let timeout = request.timeout.min(state.config.command_timeout());
            let facts = events::command_facts(&request.argv, 0);
            let started = std::time::Instant::now();
            let result = run!(
                operations::exec(state, request),
                timeout,
                SandboxOperation::Exec
            );
            let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            match &result {
                Ok(output) => events::exec_completed(
                    &facts,
                    duration_ms,
                    "ok",
                    Some(output.exit_code),
                    output.stdout.len(),
                    output.stderr.len(),
                    false,
                    None,
                ),
                Err(SandboxError::OutputLimit { phase, .. }) => {
                    let stream = match phase {
                        OutputPhase::Stdout => "stdout",
                        OutputPhase::Stderr => "stderr",
                        OutputPhase::NotStarted => "not_started",
                    };
                    events::exec_completed(
                        &facts,
                        duration_ms,
                        "error",
                        None,
                        0,
                        0,
                        true,
                        Some(stream),
                    );
                }
                Err(_) => events::exec_completed(
                    &facts,
                    duration_ms,
                    outcome_label(&result),
                    None,
                    0,
                    0,
                    false,
                    None,
                ),
            }
            record_operation(&state.metrics, OperationKind::Exec, &result);
            let _ = reply.send(result);
        }
        // Attachment allocates an exec, so a deadline or cancellation destroys
        // the owned instance instead of leaving an unobserved guest process.
        Op::Spawn { request, reply } => {
            let result = run!(
                process::spawn(state, request),
                file_timeout,
                SandboxOperation::SpawnProcess
            );
            record_operation(&state.metrics, OperationKind::SpawnProcess, &result);
            let _ = reply.send(result);
        }
        Op::Status(reply) => {
            let result = observe!(operations::status(state), lifecycle_timeout);
            record_operation(&state.metrics, OperationKind::Status, &result);
            let _ = reply.send(result);
        }
        Op::CaptureSnapshot {
            budget,
            max_manifest_bytes,
            reply,
        } => {
            let result = snapshot::capture(state, budget, max_manifest_bytes, &cancel).await;
            record_operation(
                &state.metrics,
                OperationKind::CaptureWorkspaceSnapshot,
                &result,
            );
            state.metrics.record_snapshot_capture(result.is_ok());
            events::snapshot_capture_completed(outcome_label(&result), cleanup_required(&result));
            let _ = reply.send(result);
        }
        Op::RestoreSnapshot { snapshot, reply } => {
            let result = snapshot::restore(state, snapshot, &cancel).await;
            record_operation(
                &state.metrics,
                OperationKind::RestoreWorkspaceSnapshot,
                &result,
            );
            state.metrics.record_snapshot_restore(result.is_ok());
            events::snapshot_restore_completed(outcome_label(&result), cleanup_required(&result));
            let _ = reply.send(result);
        }
    }
}

#[cfg(test)]
impl Sandbox {
    /// Builds a sandbox over an in-process test backend. Used by HTTP handler
    /// tests; it runs the same capability, preparation, and worker path as a
    /// production create, differing only in how the backend is selected.
    pub(crate) async fn create_for_test<B: Backend>(
        backend: B,
        config: SandboxConfig,
    ) -> Result<Self, SandboxError> {
        let spec = common_spec(&config);
        Self::create_with_backend(
            backend,
            spec,
            config,
            SandboxMetrics::disabled(),
            RecoveryId::generate(),
        )
        .await
    }
}

#[cfg(test)]
mod tests;
