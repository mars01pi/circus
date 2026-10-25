//! One interactive process per sandbox; only lifecycle publishes its outcome.
//!
//! The transport is backend-neutral: the worker holds the backend, so the I/O
//! task receives an already-attached `BackendProcess` instead of reaching into
//! a backend itself.

mod io;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::operations::{
    cleanup_engine_error, destroy_report, need, require_capability, required_id,
};
use super::{CleanupReport, CommandGuard, Ownership, SandboxError, SandboxPhase, State};
use crate::infrastructure::{Backend, ProcessSpec};
use crate::observability::{TerminationKind, events};
use crate::runtime::check_relative;

const CHUNK_BYTES: usize = 16 * 1024;
const BUFFER_CHUNKS: usize = 8;

/// An argv command with explicit environment overrides and bounded streaming output.
/// Host environment variables are never copied. Image environment still applies.
/// `working_dir` is a relative directory below the configured container workspace;
/// `None` uses the workspace itself. Container symlinks retain container semantics.
/// The lifetime timeout includes backpressure and time spent paused.
#[derive(Clone, PartialEq, Eq)]
pub struct ProcessRequest {
    pub argv: Vec<String>,
    pub working_dir: Option<String>,
    pub env: BTreeMap<String, String>,
    pub timeout: Duration,
    /// Maximum total bytes for each of stdout and stderr, not a per-chunk limit.
    pub output_limit_bytes: usize,
}

impl std::fmt::Debug for ProcessRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessRequest")
            .field("argc", &self.argv.len())
            .field("working_dir", &self.working_dir)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("timeout", &self.timeout)
            .field("output_limit_bytes", &self.output_limit_bytes)
            .finish()
    }
}

impl ProcessRequest {
    pub(super) fn validate(&self) -> Result<(), SandboxError> {
        let invalid = |reason| SandboxError::InvalidProcessRequest { reason };
        if self.argv.first().is_none_or(String::is_empty)
            || self.argv.iter().any(|arg| arg.contains('\0'))
        {
            return Err(invalid("argv needs a nonempty executable and no NUL bytes"));
        }
        if self.timeout.is_zero()
            || std::time::Instant::now()
                .checked_add(self.timeout)
                .is_none()
        {
            return Err(invalid(
                "process timeout must be positive and representable",
            ));
        }
        if self.output_limit_bytes == 0 || self.output_limit_bytes == usize::MAX {
            return Err(invalid("process output limit must be in 1..usize::MAX"));
        }
        if let Some(dir) = &self.working_dir {
            check_relative(dir)?;
            if dir.contains('\0') {
                return Err(invalid("working directory contains NUL"));
            }
        }
        for (key, value) in &self.env {
            let mut name = key.bytes();
            if !name
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                || !name.all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || value.contains('\0')
            {
                return Err(invalid(
                    "environment needs portable names and NUL-free values",
                ));
            }
        }
        Ok(())
    }
}

/// Raw, ordered output chunks. Chunks are not lines and may split UTF-8 characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessOutput {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

/// Why an interactive process required whole-container cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessTermination {
    Cancelled,
    TimedOut,
    OutputLimit { limit: usize },
    Transport { detail: String },
    WorkerFailed { detail: String },
}

/// A normal exit retains the sandbox and workspace, including for nonzero codes.
/// Termination reports an actual cleanup attempt; unknown ownership requires
/// retrying `Sandbox::destroy` on the retained sandbox handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessExit {
    Exited {
        exit_code: i64,
    },
    Terminated {
        reason: ProcessTermination,
        cleanup: CleanupReport,
    },
}

/// Input/notification errors. Use `wait` for the process and cleanup outcome.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProcessError {
    #[error("process stdin is closed")]
    StdinClosed,
    #[error("process I/O failed: {detail}")]
    Io { detail: String },
    #[error("lifecycle stopped without reporting a process outcome")]
    WorkerGone,
}

enum Input {
    Write(Vec<u8>, oneshot::Sender<Result<(), ProcessError>>),
    Close(oneshot::Sender<Result<(), ProcessError>>),
}

/// A single-consumer output stream with independently usable stdin and wait.
/// Drain output concurrently with writes/wait: eight 16-KiB chunks buffer before
/// backpressure. Dropping this handle or cancelling a partially accepted input
/// write requests destruction of the entire container. Drop cannot report cleanup.
pub struct ProcessSession {
    input: mpsc::Sender<Input>,
    input_lock: Mutex<()>,
    output: Mutex<mpsc::Receiver<ProcessOutput>>,
    exit: watch::Receiver<Option<ProcessExit>>,
    cancel: CancellationToken,
}

impl ProcessSession {
    /// Writes bytes in bounded chunks. Concurrent calls do not interleave.
    /// Success means the transport accepted the bytes, not that the guest read them.
    pub async fn write_stdin(&self, bytes: &[u8]) -> Result<(), ProcessError> {
        let _lock = self.input_lock.lock().await;
        let mut guard = CommandGuard {
            token: self.cancel.clone(),
            armed: false,
        };
        for chunk in bytes.chunks(CHUNK_BYTES) {
            let permit = self
                .input
                .reserve()
                .await
                .map_err(|_| ProcessError::StdinClosed)?;
            let (tx, rx) = oneshot::channel();
            guard.armed = true;
            permit.send(Input::Write(chunk.to_vec(), tx));
            let result = rx.await.map_err(|_| ProcessError::StdinClosed)?;
            if let Err(err) = result {
                guard.armed = false;
                return Err(err);
            }
        }
        guard.armed = false;
        Ok(())
    }

    /// Sends EOF without killing the process. Repeated closes succeed while the
    /// transport is alive; after exit they may return `StdinClosed`.
    pub async fn close_stdin(&self) -> Result<(), ProcessError> {
        let _lock = self.input_lock.lock().await;
        let permit = self
            .input
            .reserve()
            .await
            .map_err(|_| ProcessError::StdinClosed)?;
        let (tx, rx) = oneshot::channel();
        let mut guard = CommandGuard {
            token: self.cancel.clone(),
            armed: true,
        };
        permit.send(Input::Close(tx));
        let result = rx.await.map_err(|_| ProcessError::StdinClosed)?;
        guard.armed = false;
        result
    }

    /// Receives the next output chunk. Cancelling this wait does not cancel the
    /// process or discard a chunk. `None` closes output; consult `wait` for why.
    pub async fn next_output(&self) -> Option<ProcessOutput> {
        self.output.lock().await.recv().await
    }

    /// Waits for exit or an observed cleanup outcome. Repeatable and cancel-safe.
    /// Output must be drained concurrently to release backpressure.
    pub async fn wait(&self) -> Result<ProcessExit, ProcessError> {
        let mut exit = self.exit.clone();
        loop {
            if let Some(result) = exit.borrow_and_update().clone() {
                return Ok(result);
            }
            exit.changed().await.map_err(|_| ProcessError::WorkerGone)?;
        }
    }

    /// Requests whole-container destruction. Await `wait` to observe cleanup.
    /// Agent turn cancellation should instead be sent using its stdin protocol.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for ProcessSession {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub(super) struct ActiveProcess {
    task: Option<JoinHandle<Result<i64, ProcessTermination>>>,
    cancel: CancellationToken,
    exit: watch::Sender<Option<ProcessExit>>,
}

pub(super) async fn spawn<B: Backend>(
    state: &mut State<B>,
    request: ProcessRequest,
) -> Result<ProcessSession, SandboxError> {
    require_capability(
        state.backend.capabilities().spawn_process,
        "attach an interactive process",
    )?;
    need(state, SandboxPhase::Running, "spawn_process")?;
    if state.process.is_some() {
        return Err(SandboxError::ProcessActive);
    }
    if request.output_limit_bytes > state.config.output_limit_bytes() {
        return Err(SandboxError::InvalidProcessRequest {
            reason: "process output limit exceeds the sandbox configuration",
        });
    }
    let id = required_id(state)?;
    let base = state.config.isolation().workspace_path();
    let workdir = request
        .working_dir
        .as_ref()
        .map_or_else(|| base.to_string(), |dir| format!("{base}/{dir}"));
    let facts = events::process_facts(&request.argv, &request.env);
    let timeout_ms = u64::try_from(request.timeout.as_millis()).unwrap_or(u64::MAX);
    let process = state
        .backend
        .spawn_process(
            &id,
            ProcessSpec {
                user: &state.user,
                workdir: &workdir,
                argv: &request.argv,
                env: request
                    .env
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect(),
            },
        )
        .await;
    let process = match process {
        Ok(process) => process,
        Err(err) => return cleanup_engine_error(state, err).await,
    };
    state.metrics.record_process_started();
    events::process_opened(&facts, timeout_ms, request.output_limit_bytes);
    let (input_tx, input_rx) = mpsc::channel(BUFFER_CHUNKS);
    let (output_tx, output_rx) = mpsc::channel(BUFFER_CHUNKS);
    let (exit_tx, exit_rx) = watch::channel(None);
    let cancel = CancellationToken::new();
    let task = tokio::spawn(io::run(
        process,
        input_rx,
        output_tx,
        cancel.clone(),
        request.timeout,
        request.output_limit_bytes,
    ));
    state.process = Some(ActiveProcess {
        task: Some(task),
        cancel: cancel.clone(),
        exit: exit_tx,
    });
    Ok(ProcessSession {
        input: input_tx,
        input_lock: Mutex::new(()),
        output: Mutex::new(output_rx),
        exit: exit_rx,
        cancel,
    })
}

pub(super) async fn completed(
    process: &mut Option<ActiveProcess>,
) -> Result<i64, ProcessTermination> {
    match process.as_mut().and_then(|process| process.task.as_mut()) {
        Some(task) => task.await.unwrap_or_else(|err| {
            Err(ProcessTermination::WorkerFailed {
                detail: err.to_string(),
            })
        }),
        None => std::future::pending().await,
    }
}

pub(super) async fn finish<B: Backend>(
    state: &mut State<B>,
    result: Result<i64, ProcessTermination>,
) {
    if let Some(process) = state.process.take() {
        let (label, kind, exit_code) = match &result {
            Ok(exit_code) => ("exited", TerminationKind::Exited, Some(*exit_code)),
            Err(reason) => (termination_label(reason), termination_kind(reason), None),
        };
        let (outcome, ownership) = match result {
            Ok(exit_code) => (ProcessExit::Exited { exit_code }, None),
            Err(reason) => {
                let cleanup = destroy_report(state).await;
                let ownership = super::cleanup::ownership_label(cleanup.ownership);
                (ProcessExit::Terminated { reason, cleanup }, Some(ownership))
            }
        };
        state.metrics.record_process_ended(kind);
        events::process_closed(label, exit_code, ownership);
        process.exit.send_replace(Some(outcome));
    }
}

/// Called by the lifecycle owner before removing the container. Closing the
/// transport alone never claims the guest or its descendants have stopped.
pub(super) async fn stop<B: Backend>(state: &mut State<B>) {
    if let Some(process) = &mut state.process {
        process.cancel.cancel();
        if let Some(task) = &mut process.task {
            let _ = task.await;
            process.task = None;
        }
    }
    // Keep the completion sender in State across cancellable engine cleanup.
    // A retried destroy must still report the actual ownership to the session.
}

pub(super) fn destroyed(
    process: Option<ActiveProcess>,
    instance_id: Option<String>,
    result: &Result<(), SandboxError>,
    metrics: &crate::observability::SandboxMetrics,
) {
    if let Some(process) = process {
        let cleanup = match result {
            Ok(()) => CleanupReport {
                instance_id: None,
                ownership: Ownership::ConfirmedAbsent,
                detail: "owned instance confirmed absent and staging removed".to_string(),
            },
            // A typed cleanup failure reports the ownership it actually
            // observed; anything else leaves ownership unknown and retryable.
            Err(SandboxError::CleanupRequired {
                ownership, detail, ..
            }) => CleanupReport {
                instance_id,
                ownership: *ownership,
                detail: format!("cancellation cleanup failed: {detail}"),
            },
            Err(err) => CleanupReport {
                instance_id,
                ownership: Ownership::Unknown,
                detail: err.to_string(),
            },
        };
        let ownership = super::cleanup::ownership_label(cleanup.ownership);
        metrics.record_process_ended(TerminationKind::Cancelled);
        events::process_closed("cancelled", None, Some(ownership));
        process.exit.send_replace(Some(ProcessExit::Terminated {
            reason: ProcessTermination::Cancelled,
            cleanup,
        }));
    }
}

fn termination_label(reason: &ProcessTermination) -> &'static str {
    match reason {
        ProcessTermination::Cancelled => "cancelled",
        ProcessTermination::TimedOut => "timed_out",
        ProcessTermination::OutputLimit { .. } => "output_limit",
        ProcessTermination::Transport { .. } => "transport",
        ProcessTermination::WorkerFailed { .. } => "worker_failed",
    }
}

fn termination_kind(reason: &ProcessTermination) -> TerminationKind {
    match reason {
        ProcessTermination::Cancelled => TerminationKind::Cancelled,
        ProcessTermination::TimedOut => TerminationKind::TimedOut,
        ProcessTermination::OutputLimit { .. } => TerminationKind::OutputLimit,
        ProcessTermination::Transport { .. } => TerminationKind::Transport,
        ProcessTermination::WorkerFailed { .. } => TerminationKind::WorkerFailed,
    }
}
