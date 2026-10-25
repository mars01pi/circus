use std::collections::BTreeMap;
use std::sync::Arc;

use circus_sandbox::{CleanupReceipt, Sandbox, SandboxError};

/// Task progress. `wait` owns the final outcome after cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPhase {
    Preparing,
    Running,
    Exporting,
    Cleaning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPhase {
    Started,
    Completed,
}

/// Normalized, bounded task events. Text and tool output are untrusted data.
///
/// Never interpret these strings as host commands or host paths. The runner does
/// not automatically print or persist events. A slow consumer fails the task with
/// `EventBackpressure`; terminal results remain available separately via `wait`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEvent {
    Phase(TaskPhase),
    /// Opaque owned instance identity, useful for correlating cleanup evidence.
    SandboxCreated {
        instance_id: String,
    },
    TextDelta {
        text: String,
    },
    Tool {
        id: String,
        kind: String,
        phase: ToolPhase,
        exit_code: Option<i64>,
    },
    ToolOutput {
        id: String,
        text: String,
    },
}

/// Credential-safe failure categories. Upstream bodies/headers are never exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ModelFailure {
    #[error("model TLS verification or handshake failed")]
    Tls,
    #[error("model request timed out")]
    Timeout,
    #[error("model service rejected the request (HTTP {status})")]
    Rejected { status: u16 },
    #[error("model transport failed")]
    Transport,
    #[error("model response protocol is invalid")]
    Protocol,
    #[error("model request or response budget exceeded")]
    Budget,
}

#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("invalid task configuration or request: {0}")]
    Invalid(&'static str),
    #[error("task requires an active Tokio runtime")]
    NoRuntime,
    #[error("task was cancelled")]
    Cancelled,
    #[error("task deadline expired")]
    TimedOut,
    #[error("task event consumer exceeded the bounded queue or byte budget")]
    EventBackpressure,
    #[error("sandbox operation failed during {phase:?}: {source}")]
    Sandbox {
        phase: TaskPhase,
        #[source]
        source: Box<SandboxError>,
    },
    #[error("host controller I/O failed: {0}")]
    HostIo(#[source] std::io::Error),
    #[error(transparent)]
    Model(#[from] ModelFailure),
    #[error("agent turn failed")]
    AgentFailed,
    #[error("agent requested unsupported approval or user interaction")]
    UnsupportedInteraction,
    #[error("controller or agent protocol failed")]
    Protocol,
    #[error("agent process failed")]
    Process,
    #[error("artifact byte budget exceeded")]
    ArtifactLimit,
    #[error("host controller cleanup failed: {0}")]
    HostCleanup(#[source] std::io::Error),
    #[error("sandbox cleanup failed: {source}; task error: {task:?}")]
    Cleanup {
        task: Option<Box<TaskError>>,
        #[source]
        source: Box<SandboxError>,
    },
    /// Loading, restoring, capturing, or saving a caller-owned workspace
    /// archive failed. `task` retains a preceding execution error; cleanup still
    /// runs and is reported separately if it also fails.
    #[error("workspace archive failed during {phase:?}: {source}; task error: {task:?}")]
    Workspace {
        phase: TaskPhase,
        task: Option<Box<TaskError>>,
        #[source]
        source: Box<SandboxError>,
    },
    #[error("runner worker ended before reporting an outcome")]
    WorkerGone,
}

/// Retained ownership after failed cleanup. Cloneable; never a backend SDK type.
#[derive(Debug, Clone)]
pub struct CleanupHandle(pub(crate) CleanupOwner);

#[derive(Debug, Clone)]
pub(crate) enum CleanupOwner {
    Sandbox(Arc<Sandbox>),
    Creation(CleanupReceipt),
}

impl CleanupHandle {
    /// Retry normal destruction, or await already-owned failed-create cleanup.
    /// This is independent of the task deadline and may need an external timeout.
    /// Dropping this future does not surrender lifecycle ownership.
    pub async fn retry(&self) -> Result<(), SandboxError> {
        match &self.0 {
            CleanupOwner::Sandbox(sandbox) => sandbox.destroy().await,
            CleanupOwner::Creation(receipt) => receipt.wait().await.map(|_| ()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct TaskFailure {
    #[source]
    pub error: TaskError,
    /// `Some` means cleanup was not confirmed. Keep this handle to retry/observe.
    pub cleanup: Option<CleanupHandle>,
}

pub struct TaskOutput {
    pub task_id: String,
    /// Exact requested project-relative paths; no implicit host write-back.
    pub artifacts: BTreeMap<String, Vec<u8>>,
    /// Native agent thread id to pass as `TaskRequest::agent_session` next time.
    /// `None` when the adapter did not report one.
    pub agent_session: Option<String>,
}

impl std::fmt::Debug for TaskOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskOutput")
            .field("task_id", &self.task_id)
            .field("artifact_count", &self.artifacts.len())
            .field("agent_session", &self.agent_session.is_some())
            .finish_non_exhaustive()
    }
}

/// `Ok` implies both successful execution and confirmed sandbox cleanup.
pub type TaskOutcome = Result<TaskOutput, TaskFailure>;
