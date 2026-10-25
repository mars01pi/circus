//! In-process sandbox API. Backend SDK types are not re-exported.

pub use crate::lifecycle::{
    CleanupFailure, CleanupReceipt, CleanupReport, CommandError, CommandOutput, CommandRequest,
    EnforcementReport, OutputPhase, Ownership, ProcessError, ProcessExit, ProcessOutput,
    ProcessRequest, ProcessSession, ProcessTermination, RecoveryId, RecoveryIdError, Sandbox,
    SandboxError, SandboxOperation, SandboxPhase, SandboxStatus, StagingError,
};

pub use crate::runtime::{
    BackendConfig, BackendConfigError, ConfigError, ContainerdBackendConfig, DockerBackendConfig,
    IoLimits, IsolationConfig, KubernetesBackendConfig, SandboxConfig,
};

pub use crate::workspace::{CodeSnapshot, SnapshotBudget, SnapshotError};

pub use crate::observability::{
    MetricsSnapshot, OperationCounters, OperationKind, OperationOutcome, SandboxMetrics,
    TerminationKind,
};
