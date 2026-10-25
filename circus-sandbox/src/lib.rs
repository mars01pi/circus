//! Backend-neutral workspace files, snapshots, image preparation, and three real
//! Linux backends.
//!
//! [`Workspace`] confines file access to a directory it creates. [`Sandbox`] is
//! a separate async API: it stages those files into a bounded tmpfs and runs
//! commands in a Linux container through an explicitly selected local Docker
//! Engine endpoint, containerd daemon, or Kubernetes Pod, all with `runc`.
//! Creation takes a [`BackendConfig`] and a backend-neutral [`SandboxConfig`];
//! callers do not name backend SDK types.
//!
//! [`CodeSource`] copies an explicitly authorized source directory into a fresh
//! owned workspace. [`CodeSnapshot`] captures workspace contents in memory and
//! can publish that content as a bounded, versioned file archive with
//! [`CodeSnapshot::save_file`] or read it back with
//! [`CodeSnapshot::load_file`]; the archive stores regular files and directories
//! only and is not a process, memory, VM, or Docker-commit checkpoint. Sandbox
//! snapshots capture and restore tmpfs workspace files, including
//! running writes, but are not process, memory, VM, or Docker-commit
//! checkpoints. [`environment::prepare_docker_image`] resolves an explicit
//! reference to an immutable image identity, defaulting to local-only inspection;
//! explicit [`environment::ImagePullPolicy::IfMissing`] permits one unauthenticated
//! missing-image pull. It never loads registry credentials. [`environment::EnvironmentPlan`] adds an
//! ordered, pinned preparation plan that executes separate-argv guest commands
//! in one owned sandbox and confirms cleanup, and
//! [`environment::EnvironmentId`] derives an opaque SHA-256 identity from the
//! resolved image, platform, pinned steps, and an explicit tenant domain. The
//! identity is not a cache, registry, or persisted lookup.
//!
//! The bounded Docker pool prestarts fresh single-use containers and never
//! resets dirty leases. [`GitCheckout`] prepares a fixed commit with a controlled
//! host Git process, and [`GitCheckout::create_worktree`] adds independent
//! detached [`GitWorktree`] values at that commit. Local OCI image resolution
//! inspects an existing image.
//!
//! [`Sandbox::spawn_process`] attaches one interactive process per sandbox with
//! explicit argv, environment overrides, a workspace-relative working directory,
//! a lifetime deadline, and a bounded output budget per stream. The returned
//! [`ProcessSession`] keeps stdin, ordered stdout/stderr chunks, exit
//! notification, and cancellation independent, while files, status,
//! pause/resume, and destroy remain usable. Cancellation, timeout, output
//! overflow, transport failure, or dropping the session requests owned
//! container cleanup; a normal exit preserves container and workspace. Docker,
//! containerd, and Kubernetes attach through their native exec transports; the
//! Kubernetes trampoline passes explicit environment values in argv because
//! `pods/exec` has no environment option. A session is not a TTY, process-group
//! signal API, or detached recovery mechanism.
//!
//! A caller that must survive loss of the in-process handle can persist a
//! [`RecoveryId`], create with [`Sandbox::create_with_recovery_id`], and later
//! call [`Sandbox::recover_orphan`]. Recovery connects only the selected
//! backend, never prepares an image or creates an instance, and removes only a
//! name whose owner label matches; the returned [`Ownership`] is best effort
//! and may remain unconfirmed. There is no list-all API or durable registry.
//! Not implemented: true pause/resume, gVisor, Kata, Firecracker, and registry
//! authentication.
//!
//! The optional `http` feature (enabled by default) adds an authenticated JSON
//! transport over the same [`Sandbox`] API (`http::HttpServer`). It requires an
//! explicit bearer token and tenant allow-list,
//! scopes every lookup by tenant, and keeps a separate in-process registry per
//! service instance. It is not a second domain implementation, a readiness
//! probe for Docker, or a durable multi-process service; `GET /healthz` reports
//! only process liveness. Transport DTOs are documented in
//! `docs/http-api.md` and `docs/openapi.json`.
//!
//! # Observability
//!
//! [`observability::SandboxMetrics`] is a caller-created, cloneable handle over
//! a fixed set of atomic counters. Pass it to
//! [`Sandbox::create_with_metrics`] or [`PoolConfig::with_metrics`]; without a
//! handle, recording is a branch-only no-op. Counters are read with
//! [`observability::SandboxMetrics::snapshot`]. Lifecycle, pool, and backend
//! code also emits `tracing` events and spans with stable names and bounded
//! fields. This crate never installs a global subscriber and exposes no health,
//! readiness, or metrics HTTP endpoint; the embedding application owns both the
//! subscriber and any export. Event fields never contain environment values,
//! full argv, file contents, host paths, image credentials, or endpoint URLs.
//!
//! # Blocking
//!
//! Workspace methods block the calling thread. [`Sandbox`] methods are async and
//! must be called from a Tokio runtime. They run workspace I/O with
//! `spawn_blocking`.
//!
//! # Limits
//!
//! [`Workspace::new`] takes a positive byte cap for a single file. It is not a
//! workspace total, file-count, or host disk quota, and this crate has no
//! total-quota API. Reads never allocate from untrusted file metadata and stop
//! after proving the file exceeds the cap. Writes that exceed the cap fail
//! before any create or truncate. A closed workspace, a rejected path, a
//! missing parent, or a non-regular node also returns before the file is
//! created or truncated. Replacing a regular file is not atomic and not
//! crash-safe: after it is opened, its length is set to zero and then the
//! bytes are written. An I/O error after that open can leave the file
//! truncated or only partly written. This crate has no atomic-write API.
//!
//! Calls on one [`Workspace`] share an internal lock so a read does not observe
//! another call on that value mid-truncate. That is not process isolation:
//! other processes can still change the directory.
//!
//! ```
//! use circus_sandbox::Workspace;
//!
//! let mut workspace = Workspace::new(64)?;
//! workspace.write_file("note.txt", b"hello")?;
//! assert_eq!(workspace.read_file("note.txt")?, b"hello");
//! workspace.remove_file("note.txt")?;
//! workspace.cleanup()?;
//! # Ok::<(), circus_sandbox::WorkspaceError>(())
//! ```

#![forbid(unsafe_code)]

pub mod api;
pub mod environment;
#[cfg(feature = "http")]
pub mod http;
mod infrastructure;
mod lifecycle;
pub mod observability;
pub mod pool;
pub mod runtime;
mod workspace;

#[doc(inline)]
pub use api::{
    BackendConfig, BackendConfigError, CleanupFailure, CleanupReceipt, CleanupReport, CommandError,
    CommandOutput, CommandRequest, ConfigError, ContainerdBackendConfig, DockerBackendConfig,
    EnforcementReport, IoLimits, IsolationConfig, KubernetesBackendConfig, OutputPhase, Ownership,
    ProcessError, ProcessExit, ProcessOutput, ProcessRequest, ProcessSession, ProcessTermination,
    RecoveryId, RecoveryIdError, Sandbox, SandboxConfig, SandboxError, SandboxOperation,
    SandboxPhase, SandboxStatus, StagingError,
};
#[doc(inline)]
pub use workspace::{
    CodeCopyError, CodeSnapshot, CodeSource, GitCheckout, GitCleanup, GitError, GitOptions,
    GitRevision, GitSource, GitWorktree, PathRejectReason, SnapshotBudget, SnapshotError,
    Workspace, WorkspaceError,
};

#[doc(inline)]
pub use runtime::{ResourceLimits, ResourceLimitsError};

#[doc(inline)]
pub use pool::{PoolConfig, PoolError, PoolFailure, PoolLease, PoolStatus, SandboxPool};

#[doc(inline)]
pub use observability::{
    MetricsSnapshot, OperationCounters, OperationKind, OperationOutcome, SandboxMetrics,
    TerminationKind,
};
