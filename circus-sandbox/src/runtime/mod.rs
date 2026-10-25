//! Backend-neutral resource, isolation, and I/O request configuration.
//!
//! A valid [`ResourceLimits`] value is not evidence that resources are enforced,
//! that execution is isolated, or that aggregate admission across requests has
//! succeeded. Backend enforcement and admission control are separate concerns;
//! this module does not start or configure a runtime. [`SandboxConfig`] carries
//! backend-neutral requirements; [`BackendConfig`] selects the backend and
//! carries its own descriptor.

mod config;

use thiserror::Error;

pub use config::{
    BackendConfig, BackendConfigError, ConfigError, ContainerdBackendConfig, DockerBackendConfig,
    IoLimits, IsolationConfig, KubernetesBackendConfig, SandboxConfig,
};

pub(crate) use config::check_relative;

/// Failure of a privately owned preparation process, independent of its SDK.
#[derive(Debug, Error)]
pub enum ProcessError {
    /// An operating-system operation failed.
    #[error("process {operation} failed: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    /// The whole operation exceeded its deadline.
    #[error("process deadline exceeded")]
    Timeout,
    /// The owner requested cancellation.
    #[error("process cancelled")]
    Cancelled,
    /// Combined stdout and stderr exceeded the capture budget.
    #[error("process output exceeded {limit} bytes")]
    OutputLimit { limit: usize },
    /// Termination or resource draining failed. The process owner remains alive.
    /// Await `cleanup.wait()` before deleting any directory the process can use.
    #[error("process cleanup failed: {source}")]
    Cleanup {
        primary: Option<Box<ProcessError>>,
        #[source]
        source: std::io::Error,
        cleanup: ProcessCleanup,
    },
}

/// A retained process owner's completion notification; contains no host paths.
///
/// Dropping this handle does not stop cleanup. The private owner retries failures
/// with a delay, retaining the child identity until termination can be proved.
#[derive(Clone, Debug)]
pub struct ProcessCleanup {
    pub(crate) done: tokio::sync::watch::Receiver<bool>,
}

impl ProcessCleanup {
    /// Waits for confirmed process-tree cleanup. This may wait indefinitely if
    /// the operating system cannot confirm termination; it never reports a
    /// failed cleanup as success.
    pub async fn wait(&self) {
        let mut done = self.done.clone();
        while !*done.borrow_and_update() {
            if done.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

/// A validated, backend-neutral resource request for one sandbox.
///
/// Memory and workspace sizes are positive byte counts representable by a
/// signed 64-bit backend or durable numeric field. Workspace bytes must not
/// exceed memory bytes. The per-file byte limit must be in `1..usize::MAX`
/// and must not exceed the aggregate workspace byte limit, matching the
/// [`crate::Workspace`] single-file limit bounds.
///
/// CPU is measured in thousandths of one CPU (`1000` means one CPU), not elapsed
/// execution time. CPU and process counts must be positive. Backend-specific
/// minima and host capacity are deliberately not part of this value model.
///
/// Construction validates only the request. It does not enforce these limits,
/// provide isolation, reserve resources, or perform aggregate admission control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceLimits {
    memory_bytes: u64,
    cpu_millis: u32,
    pids: u32,
    workspace_bytes: u64,
    max_file_bytes: usize,
}

impl ResourceLimits {
    /// Validates a resource request without allocating or acquiring resources.
    ///
    /// # Errors
    ///
    /// Returns a typed error for the first invalid field or relationship. Scalar
    /// bounds are checked before relationships, so an out-of-range workspace
    /// size is reported before its relationship with memory is considered.
    pub fn new(
        memory_bytes: u64,
        cpu_millis: u32,
        pids: u32,
        workspace_bytes: u64,
        max_file_bytes: usize,
    ) -> Result<Self, ResourceLimitsError> {
        if memory_bytes == 0 {
            return Err(ResourceLimitsError::ZeroMemory);
        }
        if memory_bytes > i64::MAX as u64 {
            return Err(ResourceLimitsError::MemoryOutOfRange { memory_bytes });
        }
        if cpu_millis == 0 {
            return Err(ResourceLimitsError::ZeroCpu);
        }
        if pids == 0 {
            return Err(ResourceLimitsError::ZeroPids);
        }
        if workspace_bytes == 0 {
            return Err(ResourceLimitsError::ZeroWorkspace);
        }
        if workspace_bytes > i64::MAX as u64 {
            return Err(ResourceLimitsError::WorkspaceOutOfRange { workspace_bytes });
        }
        if max_file_bytes == 0 || max_file_bytes == usize::MAX {
            return Err(ResourceLimitsError::InvalidMaxFileBytes { max_file_bytes });
        }
        if workspace_bytes > memory_bytes {
            return Err(ResourceLimitsError::WorkspaceExceedsMemory {
                workspace_bytes,
                memory_bytes,
            });
        }
        // Widen both operands: never truncate a platform-sized per-file limit.
        if max_file_bytes as u128 > u128::from(workspace_bytes) {
            return Err(ResourceLimitsError::MaxFileExceedsWorkspace {
                max_file_bytes,
                workspace_bytes,
            });
        }
        Ok(Self {
            memory_bytes,
            cpu_millis,
            pids,
            workspace_bytes,
            max_file_bytes,
        })
    }

    /// Returns the requested memory limit in bytes.
    pub fn memory_bytes(&self) -> u64 {
        self.memory_bytes
    }

    /// Returns the requested CPU capacity in thousandths of one CPU.
    pub fn cpu_millis(&self) -> u32 {
        self.cpu_millis
    }

    /// Returns the requested maximum number of processes.
    pub fn pids(&self) -> u32 {
        self.pids
    }

    /// Returns the requested aggregate workspace limit in bytes.
    pub fn workspace_bytes(&self) -> u64 {
        self.workspace_bytes
    }

    /// Returns the per-file byte limit, in `1..usize::MAX`.
    pub fn max_file_bytes(&self) -> usize {
        self.max_file_bytes
    }
}

/// A resource request violates a scalar bound or a linked resource constraint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum ResourceLimitsError {
    /// Memory must be positive.
    #[error("memory_bytes must be positive")]
    ZeroMemory,
    /// Memory must fit a signed 64-bit numeric field.
    #[error("memory_bytes {memory_bytes} exceeds i64::MAX")]
    MemoryOutOfRange { memory_bytes: u64 },
    /// CPU capacity must be positive.
    #[error("cpu_millis must be positive")]
    ZeroCpu,
    /// The process limit must be positive.
    #[error("pids must be positive")]
    ZeroPids,
    /// Aggregate workspace capacity must be positive.
    #[error("workspace_bytes must be positive")]
    ZeroWorkspace,
    /// Aggregate workspace capacity must fit a signed 64-bit numeric field.
    #[error("workspace_bytes {workspace_bytes} exceeds i64::MAX")]
    WorkspaceOutOfRange { workspace_bytes: u64 },
    /// The per-file limit must be positive and below [`usize::MAX`].
    #[error("max_file_bytes must be in 1..usize::MAX, got {max_file_bytes}")]
    InvalidMaxFileBytes { max_file_bytes: usize },
    /// The workspace request exceeds the memory request.
    #[error("workspace_bytes {workspace_bytes} exceeds memory_bytes {memory_bytes}")]
    WorkspaceExceedsMemory {
        workspace_bytes: u64,
        memory_bytes: u64,
    },
    /// The per-file limit exceeds the aggregate workspace limit.
    #[error("max_file_bytes {max_file_bytes} exceeds workspace_bytes {workspace_bytes}")]
    MaxFileExceedsWorkspace {
        max_file_bytes: usize,
        workspace_bytes: u64,
    },
}

#[cfg(test)]
mod tests;
