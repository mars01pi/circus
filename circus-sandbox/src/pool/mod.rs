//! Bounded, prestarted, single-use sandboxes for Docker and containerd.
//!
//! Every lease gets a fresh container. Returned containers are destroyed, never
//! reset or offered to another caller. A baseline is explicitly shared workspace
//! payload, not a process/memory checkpoint or a credential scanner. Container
//! resource limits do not reserve aggregate host capacity. Backend startup
//! still happens during warming and refill; this API makes no cold-start claim.

mod lease;
mod worker;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::observability::{OperationKind, OperationOutcome, events};
use crate::{
    BackendConfig, CodeSnapshot, SandboxConfig, SandboxError, SandboxMetrics, SandboxPhase,
};
pub use lease::PoolLease;

/// Immutable recipe used for every fresh container in a pool.
#[derive(Clone)]
pub struct PoolConfig {
    backend: BackendConfig,
    sandbox: SandboxConfig,
    capacity: usize,
    baseline: Option<Arc<CodeSnapshot>>,
    lease_ttl: Duration,
    metrics: SandboxMetrics,
}

impl PoolConfig {
    /// Validates a positive capacity and a positive, representable lease TTL.
    /// TTL starts at handoff, is not extended by activity, and revokes even an
    /// in-flight command at expiry. Baselines are authorized shared payload
    /// restored before start, never captured from a lease.
    ///
    /// `backend` is the same explicit backend descriptor used by
    /// [`crate::Sandbox::create`]; `sandbox` carries backend-neutral limits,
    /// isolation requirements, and I/O bounds.
    pub fn new(
        backend: BackendConfig,
        sandbox: SandboxConfig,
        capacity: usize,
        baseline: Option<Arc<CodeSnapshot>>,
        lease_ttl: Duration,
    ) -> Result<Self, PoolError> {
        if capacity == 0 {
            return Err(PoolError::InvalidCapacity);
        }
        if lease_ttl.is_zero() || tokio::time::Instant::now().checked_add(lease_ttl).is_none() {
            return Err(PoolError::InvalidLeaseTtl);
        }
        Ok(Self {
            backend,
            sandbox,
            capacity,
            baseline,
            lease_ttl,
            metrics: SandboxMetrics::disabled(),
        })
    }

    /// Attaches caller-owned counters to every sandbox this pool creates.
    ///
    /// The handle is cloned into the pool worker. Every prestarted or refilled
    /// sandbox reports its create and lifecycle activity to the same handle, so
    /// pool activity is visible in one snapshot. Pool slot transitions are
    /// recorded as cumulative counts; use [`SandboxPool::status`] for current
    /// gauges. This builder does not allocate a tracing subscriber.
    pub fn with_metrics(mut self, metrics: SandboxMetrics) -> Self {
        self.metrics = metrics;
        self
    }
}

/// A coherent actor snapshot. Categories are disjoint. `absent` counts slots
/// whose containers are confirmed absent during shutdown; all categories sum
/// to `capacity`. Cleanup failures continue occupying their original slot:
/// `retiring` means destruction is in progress and no attempt has failed yet,
/// while `cleanup_required` means the latest attempt could not confirm absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolStatus {
    pub capacity: usize,
    pub ready: usize,
    pub creating: usize,
    pub leased: usize,
    pub retiring: usize,
    pub cleanup_required: usize,
    pub absent: usize,
    pub closing: bool,
}

/// Retained cause of a pool operation failure.
#[derive(Debug, thiserror::Error)]
pub enum PoolFailure {
    #[error("sandbox {operation} failed: {source}")]
    Sandbox {
        operation: &'static str,
        #[source]
        source: SandboxError,
    },
    #[error("new sandbox reported {phase:?}, not Running")]
    NotRunning { phase: SandboxPhase },
}

/// Typed pool errors. Failed construction transfers the cleanup client back to
/// the caller when absence cannot be confirmed. Keep it and retry `shutdown`.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("pool capacity must be positive")]
    InvalidCapacity,
    #[error("lease TTL must be positive and representable")]
    InvalidLeaseTtl,
    #[error("acquire timeout must be positive and representable")]
    InvalidTimeout,
    #[error("pool acquire deadline expired")]
    AcquireTimeout,
    #[error("pool is closing")]
    Closing,
    #[error("pool worker stopped unexpectedly")]
    WorkerGone,
    #[error("pool operation failed: {source}")]
    Operation {
        #[source]
        source: Arc<PoolFailure>,
    },
    #[error("construction failed and cleanup remains owned: {source}")]
    CleanupRequired {
        pool: Box<SandboxPool>,
        #[source]
        source: Arc<PoolFailure>,
    },
}

struct Client {
    tx: mpsc::UnboundedSender<worker::Request>,
    close: CancellationToken,
    metrics: SandboxMetrics,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.close.cancel();
    }
}

/// Cloneable client for a single bounded pool; leases are not cloneable.
///
/// The pool is backend-neutral: it creates each container through the
/// [`crate::Sandbox`] abstraction, so the same type serves Docker and
/// containerd descriptors. Last-client drop requests shutdown but cannot prove
/// cleanup succeeded. Lease handles do not keep that drop guard alive. The
/// worker retains all containers and retries failed removal independently of
/// request futures.
#[derive(Clone)]
pub struct SandboxPool {
    client: Arc<Client>,
}

impl fmt::Debug for SandboxPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxPool").finish_non_exhaustive()
    }
}

impl SandboxPool {
    /// Warms the entire capacity, restoring payload snapshots before starting
    /// and checking the actual Running status of every container. Cancellation
    /// requests shutdown without dropping in-flight creates.
    pub async fn create(config: PoolConfig) -> Result<Self, PoolError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let close = CancellationToken::new();
        let metrics = config.metrics.clone();
        let pool = Self {
            client: Arc::new(Client {
                tx: tx.clone(),
                close: close.clone(),
                metrics: metrics.clone(),
            }),
        };
        let (ready, result) = oneshot::channel();
        tokio::spawn(worker::run(config, rx, tx, close, ready));
        match result.await.map_err(|_| PoolError::WorkerGone)? {
            Ok(()) => Ok(pool),
            Err(source) => match pool.shutdown().await {
                Ok(()) => Err(PoolError::Operation { source }),
                Err(_) => Err(PoolError::CleanupRequired {
                    pool: Box::new(pool),
                    source,
                }),
            },
        }
    }

    /// Waits FIFO among live waiters for a single-use lease. The explicit,
    /// positive deadline includes queueing; cancelled waiters never own a slot.
    pub async fn acquire(&self, timeout: Duration) -> Result<PoolLease, PoolError> {
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .filter(|_| !timeout.is_zero())
            .ok_or(PoolError::InvalidTimeout)?;
        let result = async {
            let (reply, result) = oneshot::channel();
            let cancelled = CancellationToken::new();
            let _guard = CancelWaiter(cancelled.clone());
            self.client
                .tx
                .send(worker::Request::Acquire {
                    reply,
                    cancelled,
                    deadline,
                })
                .map_err(|_| PoolError::WorkerGone)?;
            tokio::time::timeout_at(deadline, result)
                .await
                .map_err(|_| PoolError::AcquireTimeout)?
                .map_err(|_| PoolError::WorkerGone)?
        }
        .await;
        self.client
            .metrics
            .record_operation(OperationKind::PoolAcquire, pool_outcome(&result));
        events::pool_acquire_completed(pool_outcome_label(&result));
        result
    }

    /// Returns disjoint slot counts without blocking behind slow Docker calls.
    pub async fn status(&self) -> Result<PoolStatus, PoolError> {
        let (reply, result) = oneshot::channel();
        self.client
            .tx
            .send(worker::Request::Status(reply))
            .map_err(|_| PoolError::WorkerGone)?;
        result.await.map_err(|_| PoolError::WorkerGone)
    }

    /// Closes admission, cancels queued waiters, and **revokes outstanding
    /// leases**, destroying their containers even while lease handles remain.
    /// Success means every owned container is confirmed absent. Idempotent;
    /// cancelling this future does not stop cleanup. A cleanup error retains
    /// ownership and retries in the background; call again to await absence.
    pub async fn shutdown(&self) -> Result<(), PoolError> {
        self.client.close.cancel();
        let (reply, result) = oneshot::channel();
        self.client
            .tx
            .send(worker::Request::Shutdown(reply))
            .map_err(|_| PoolError::WorkerGone)?;
        result.await.map_err(|_| PoolError::WorkerGone)?
    }
}

struct CancelWaiter(CancellationToken);
impl Drop for CancelWaiter {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn pool_outcome(result: &Result<PoolLease, PoolError>) -> OperationOutcome {
    match result {
        Ok(_) => OperationOutcome::Ok,
        Err(PoolError::AcquireTimeout) => OperationOutcome::TimedOut,
        Err(_) => OperationOutcome::Error,
    }
}

fn pool_outcome_label(result: &Result<PoolLease, PoolError>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(PoolError::AcquireTimeout) => "timed_out",
        Err(_) => "error",
    }
}
