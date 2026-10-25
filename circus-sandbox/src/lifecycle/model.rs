//! Public lifecycle models and matchable errors.

use std::time::Duration;

use crate::runtime::{BackendConfigError, ConfigError};
use crate::workspace::{SnapshotError, WorkspaceError};

use super::cleanup::CleanupReceipt;

/// Lifecycle phase. Only the worker mutates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxPhase {
    Created,
    Running,
    Paused,
    Destroyed,
    CleanupRequired,
}

/// Where the owned instance is after a cleanup attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    ConfirmedAbsent,
    StillPresent,
    Unknown,
}

/// Caller-persistable identity for one owned backend instance.
///
/// A value is exactly 36 bytes: lowercase hexadecimal digits with hyphens at
/// positions 8, 13, 18, and 23. [`generate`](Self::generate) uses a random
/// UUID v4. The value is a stable label, not a secret or an authentication
/// token; its `Debug` output therefore shows the string.
///
/// Persist the value together with the backend descriptor before calling
/// [`Sandbox::create_with_recovery_id`](crate::Sandbox::create_with_recovery_id),
/// and obtain generated values from
/// [`Sandbox::recovery_id`](crate::Sandbox::recovery_id). An ID identifies the
/// name `circus-sandbox-{id}` and the owner label; it is not a capability that
/// authorizes deletion by itself. Recovery is best effort and reports
/// [`Ownership::StillPresent`] or [`Ownership::Unknown`] instead of claiming
/// success.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecoveryId(String);

/// Why a [`RecoveryId`] was rejected.
///
/// The invalid input is never echoed, so a rejected value cannot smuggle
/// arbitrary text into an error report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("recovery id must be a canonical lowercase hyphenated UUID")]
pub struct RecoveryIdError;

impl RecoveryId {
    /// Generates a random UUID v4 in canonical form.
    #[must_use]
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Parses a canonical 36-byte lowercase hyphenated UUID.
    ///
    /// Non-canonical spellings (uppercase, missing hyphens, `urn:` or brace
    /// prefixes, whitespace, or path characters) are rejected before any
    /// backend is contacted.
    pub fn parse(value: &str) -> Result<Self, RecoveryIdError> {
        match uuid::Uuid::parse_str(value) {
            Ok(uuid) if uuid.hyphenated().to_string() == value => Ok(Self(value.to_string())),
            _ => Err(RecoveryIdError),
        }
    }

    /// Canonical string form used in the instance name and owner label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for RecoveryId {
    type Err = RecoveryIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl AsRef<str> for RecoveryId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for RecoveryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Operation a caller-visible deadline or cancellation is attributed to.
///
/// This distinguishes which configured deadline fired. See
/// [`crate::runtime::IoLimits`] for which classes destroy the owned instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxOperation {
    Start,
    Pause,
    Resume,
    Status,
    WriteFile,
    ReadFile,
    CreateDirAll,
    RemoveFile,
    Exec,
    SpawnProcess,
    CaptureWorkspaceSnapshot,
    RestoreWorkspaceSnapshot,
}

impl std::fmt::Display for SandboxOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Start => "start",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Status => "status",
            Self::WriteFile => "write_file",
            Self::ReadFile => "read_file",
            Self::CreateDirAll => "create_dir_all",
            Self::RemoveFile => "remove_file",
            Self::Exec => "exec",
            Self::SpawnProcess => "spawn_process",
            Self::CaptureWorkspaceSnapshot => "capture_workspace_snapshot",
            Self::RestoreWorkspaceSnapshot => "restore_workspace_snapshot",
        };
        f.write_str(name)
    }
}

/// Stream whose next chunk would exceed the shared stdout+stderr budget.
///
/// `NotStarted` means the request was rejected before execution because its
/// declared output bound exceeds the configured bound; no byte was collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputPhase {
    NotStarted,
    Stdout,
    Stderr,
}

impl std::fmt::Display for OutputPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::NotStarted => "not started",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        };
        f.write_str(name)
    }
}

/// Why owned cleanup could not confirm absence.
///
/// This is the standard error source of [`SandboxError::CleanupRequired`] and
/// contains no backend SDK type. A retry can succeed after any of these.
#[derive(Debug, thiserror::Error)]
pub enum CleanupFailure {
    /// A backend operation failed before either absence or presence could be
    /// confirmed. The retained instance identity and staging are still owned.
    #[error("backend removal failed: {message}")]
    Backend { message: String },
    /// Forced removal was attempted, but inspection still confirms the instance.
    #[error("instance {instance_id} is still present: {detail}")]
    StillPresent { instance_id: String, detail: String },
    /// Neither absence nor presence could be confirmed.
    #[error("instance ownership is unknown: {detail}")]
    Unknown { detail: String },
    /// Creation never settled, so confirmed absence now cannot rule out a late
    /// instance. Owned cleanup continues.
    #[error("creation of instance {instance_id} is unresolved; absence is not final")]
    UnresolvedCreate { instance_id: String },
    /// The instance is gone but local staging could not be removed. Staging is
    /// still owned and retryable.
    #[error("staging cleanup failed: {0}")]
    Staging(#[from] WorkspaceError),
}

/// Cleanup outcome. Success is never implied when ownership is not confirmed absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupReport {
    /// Backend instance identity retained for retry. `None` only when the
    /// instance is confirmed absent and staging cleanup completed.
    pub instance_id: Option<String>,
    pub ownership: Ownership,
    pub detail: String,
}

/// Backend-reported effective configuration for one instance.
///
/// Every field is inspected evidence, not the request that was sent. `None`
/// means the backend did not report that property. The lifecycle owner never
/// substitutes the requested value for enforcement evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnforcementReport {
    pub runtime: Option<String>,
    pub memory_bytes: Option<i64>,
    pub nano_cpus: Option<i64>,
    pub pids_limit: Option<i64>,
    pub readonly_root: Option<bool>,
    pub network_mode: Option<String>,
    pub cap_drop: Option<Vec<String>>,
    pub security_opt: Option<Vec<String>>,
    pub bind_count: Option<usize>,
    pub tmpfs_workspace: Option<String>,
}

/// Current phase, owned instance identity, and inspected enforcement evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxStatus {
    pub phase: SandboxPhase,
    /// Opaque backend instance identity while one is retained.
    pub instance_id: Option<String>,
    /// `None` before an instance exists or after confirmed removal. This is
    /// evidence observed from the backend, never a copy of requested limits.
    pub enforcement: Option<EnforcementReport>,
}

/// One exec. `argv` is passed as separate arguments, not a shell string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRequest {
    /// Separate arguments; must include at least the executable.
    pub argv: Vec<String>,
    /// Requested non-zero command deadline. The worker also applies the
    /// configured [`crate::runtime::IoLimits::command_timeout`] ceiling and
    /// enforces whichever deadline is shorter.
    pub timeout: Duration,
    /// Non-zero captured stdout+stderr byte bound. Both streams share this
    /// single budget; exactly-at-limit output is allowed.
    pub output_limit_bytes: usize,
}

/// Finished exec. A non-zero exit code is still `Ok`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub exit_code: i64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Matchable failure. Backend SDK types are not part of this enum.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// Interactive process input was rejected before any exec was created.
    #[error("invalid process request: {reason}")]
    InvalidProcessRequest { reason: &'static str },
    /// One interactive process is already attached to this sandbox.
    #[error("an interactive process is already active in this sandbox")]
    ProcessActive,
    /// Backend-neutral configuration was rejected.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The backend descriptor was rejected before allocation.
    #[error(transparent)]
    BackendConfig(#[from] BackendConfigError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// Workspace snapshot failure, including retained temporary cleanup ownership.
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    /// An interrupted snapshot also failed explicit temporary workspace cleanup.
    #[error("{interruption}; snapshot cleanup: {snapshot}")]
    SnapshotInterrupted {
        interruption: Box<SandboxError>,
        #[source]
        snapshot: Box<SnapshotError>,
    },
    /// Invalid command input. No command is queued and ownership is unchanged.
    #[error("invalid command request: {0}")]
    Command(#[from] super::CommandError),
    /// Staged content exceeds its byte bound or cannot be counted.
    #[error("workspace staging failed: {0}")]
    Staging(#[from] super::StagingError),
    #[error("phase {current:?} cannot {operation}")]
    InvalidPhase {
        current: SandboxPhase,
        operation: &'static str,
    },
    /// The selected backend has no local copy of this image. Sandbox creation never
    /// pulls; supply an image that already exists or resolve one explicitly
    /// with [`crate::environment`].
    #[error("backend image {image} is not available locally; sandbox creation does not pull")]
    BackendImageUnavailable { image: String },
    /// A backend operation failed. The message is backend-supplied text; no SDK
    /// type is exposed.
    #[error("backend operation failed: {message}")]
    Backend { message: String },
    #[error("{operation} timed out; instance ownership is {ownership:?}: {detail}")]
    TimedOut {
        instance_id: Option<String>,
        ownership: Ownership,
        /// Operation whose deadline fired. `Status` never destroys the instance.
        operation: SandboxOperation,
        detail: String,
    },
    #[error("{operation} cancelled; instance ownership is {ownership:?}: {detail}")]
    Cancelled {
        instance_id: Option<String>,
        ownership: Ownership,
        operation: SandboxOperation,
        detail: String,
    },
    #[error("output exceeded {limit} bytes on {phase}; instance ownership is {ownership:?}")]
    OutputLimit {
        /// The request's shared stdout+stderr budget, in bytes.
        limit: usize,
        /// Stream that would have exceeded the shared budget, or `NotStarted`
        /// when the request was rejected before execution.
        phase: OutputPhase,
        ownership: Ownership,
        detail: String,
    },
    /// Creation completed but the inspected effective policy differs from the
    /// requested policy. The container was destroyed, or cleanup ownership is
    /// retained through [`Self::CreationCleanupRequired`].
    #[error("effective policy mismatch for {field}: expected {expected}, got {actual}")]
    PolicyMismatch {
        field: String,
        expected: String,
        actual: String,
    },
    /// Creation failed without returning a sandbox handle. The worker retains
    /// instance and staging ownership; await `cleanup.wait()` before reusing
    /// capacity. Dropping the receipt does not stop cleanup.
    #[error("creation failed; cleanup for instance {instance_id} continues: {source}")]
    CreationCleanupRequired {
        instance_id: String,
        #[source]
        source: Box<SandboxError>,
        cleanup: CleanupReceipt,
    },
    /// Cleanup did not confirm both instance absence and staging removal. The
    /// sandbox handle keeps the retained identity and staging; call `destroy`
    /// again. `ownership` is the last observed instance state.
    #[error("cleanup failed; instance {instance_id} is {ownership:?}: {detail}")]
    CleanupRequired {
        instance_id: String,
        ownership: Ownership,
        detail: String,
        #[source]
        failure: CleanupFailure,
    },
    #[error("sandbox worker is gone: {detail}")]
    WorkerGone { detail: String },
    #[error("relative path rejected: {reason}")]
    RelativePath { reason: &'static str },
    #[error("the selected backend cannot {capability}")]
    UnsupportedCapability { capability: &'static str },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_id_parse_display_and_as_ref() {
        let id = RecoveryId::parse("550e8400-e29b-41d4-a716-446655440000").expect("canonical");
        assert_eq!(id.as_str(), "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(id.as_ref(), id.as_str());
        assert_eq!(id.to_string(), id.as_str());
        assert_eq!(
            "550e8400-e29b-41d4-a716-446655440000"
                .parse::<RecoveryId>()
                .expect("from_str")
                .as_str(),
            id.as_str()
        );
        assert!(RecoveryId::parse("550E8400-E29B-41D4-A716-446655440000").is_err());
        assert!(RecoveryId::parse("not-a-uuid").is_err());
    }

    #[test]
    fn sandbox_operation_display_names_every_variant() {
        let cases = [
            (SandboxOperation::Start, "start"),
            (SandboxOperation::Pause, "pause"),
            (SandboxOperation::Resume, "resume"),
            (SandboxOperation::Status, "status"),
            (SandboxOperation::WriteFile, "write_file"),
            (SandboxOperation::ReadFile, "read_file"),
            (SandboxOperation::CreateDirAll, "create_dir_all"),
            (SandboxOperation::RemoveFile, "remove_file"),
            (SandboxOperation::Exec, "exec"),
            (SandboxOperation::SpawnProcess, "spawn_process"),
            (
                SandboxOperation::CaptureWorkspaceSnapshot,
                "capture_workspace_snapshot",
            ),
            (
                SandboxOperation::RestoreWorkspaceSnapshot,
                "restore_workspace_snapshot",
            ),
        ];
        for (operation, name) in cases {
            assert_eq!(operation.to_string(), name);
        }
    }

    #[test]
    fn output_phase_and_cleanup_failure_display() {
        assert_eq!(OutputPhase::NotStarted.to_string(), "not started");
        assert_eq!(OutputPhase::Stdout.to_string(), "stdout");
        assert_eq!(OutputPhase::Stderr.to_string(), "stderr");
        assert!(
            CleanupFailure::Backend {
                message: "boom".into()
            }
            .to_string()
            .contains("boom")
        );
        assert!(
            CleanupFailure::StillPresent {
                instance_id: "id".into(),
                detail: "present".into(),
            }
            .to_string()
            .contains("present")
        );
        assert!(
            CleanupFailure::Unknown {
                detail: "maybe".into()
            }
            .to_string()
            .contains("maybe")
        );
        assert!(
            CleanupFailure::UnresolvedCreate {
                instance_id: "id".into()
            }
            .to_string()
            .contains("unresolved")
        );
    }
}
