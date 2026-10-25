//! Caller-owned, bounded counters for lifecycle and pool activity.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Operation dimension for [`SandboxMetrics::record_operation`].
///
/// The set is fixed and carries no tenant, image, or caller-provided label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum OperationKind {
    Start,
    Pause,
    Resume,
    Destroy,
    Status,
    WriteFile,
    ReadFile,
    CreateDirAll,
    RemoveFile,
    Exec,
    SpawnProcess,
    CaptureWorkspaceSnapshot,
    RestoreWorkspaceSnapshot,
    PoolAcquire,
}

impl OperationKind {
    /// Number of variants, used by [`MetricsSnapshot::operations`].
    pub const COUNT: usize = 14;

    fn index(self) -> usize {
        self as usize
    }

    /// Stable lowercase label for this operation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Destroy => "destroy",
            Self::Status => "status",
            Self::WriteFile => "write_file",
            Self::ReadFile => "read_file",
            Self::CreateDirAll => "create_dir_all",
            Self::RemoveFile => "remove_file",
            Self::Exec => "exec",
            Self::SpawnProcess => "spawn_process",
            Self::CaptureWorkspaceSnapshot => "capture_workspace_snapshot",
            Self::RestoreWorkspaceSnapshot => "restore_workspace_snapshot",
            Self::PoolAcquire => "pool_acquire",
        }
    }
}

/// Outcome dimension for [`SandboxMetrics::record_operation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum OperationOutcome {
    Ok,
    Error,
    Cancelled,
    TimedOut,
}

/// How one interactive process session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum TerminationKind {
    /// The guest process exited on its own, including with a nonzero code.
    Exited,
    /// Cancellation, session drop, or sandbox destroy ended the session.
    Cancelled,
    TimedOut,
    OutputLimit,
    Transport,
    WorkerFailed,
}

impl TerminationKind {
    /// Number of variants, used by [`MetricsSnapshot::process_terminations`].
    pub const COUNT: usize = 6;

    fn index(self) -> usize {
        self as usize
    }

    /// Stable lowercase label for this termination kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::OutputLimit => "output_limit",
            Self::Transport => "transport",
            Self::WorkerFailed => "worker_failed",
        }
    }
}

/// One operation kind's outcome counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OperationCounters {
    pub ok: u64,
    pub error: u64,
    pub cancelled: u64,
    pub timed_out: u64,
}

/// Plain snapshot of every counter. Counter values saturate at `u64::MAX`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub creates_ok: u64,
    pub creates_failed: u64,
    /// Current creations whose owned cleanup is still owed. This is a gauge;
    /// it never goes below zero.
    pub create_cleanup_pending: u64,
    /// Creations whose owned cleanup was confirmed completed.
    pub create_cleanup_completed: u64,
    pub operations: [OperationCounters; OperationKind::COUNT],
    pub cleanup_attempts: u64,
    pub cleanup_failures: u64,
    /// Slots that reached `Ready` (cumulative transition count, not a gauge).
    pub pool_ready: u64,
    /// Leases handed off to callers (cumulative transition count).
    pub pool_leased: u64,
    /// Slots retired for destruction (cumulative transition count).
    pub pool_retired: u64,
    /// Slots recreated after the initial warm-up (cumulative transition count).
    pub pool_refilled: u64,
    pub snapshot_captures_ok: u64,
    pub snapshot_captures_failed: u64,
    pub snapshot_restores_ok: u64,
    pub snapshot_restores_failed: u64,
    pub process_sessions_started: u64,
    /// Sessions ended, indexed by [`TerminationKind`].
    pub process_terminations: [u64; TerminationKind::COUNT],
}

impl MetricsSnapshot {
    /// Counters for one operation kind.
    pub fn operation(&self, kind: OperationKind) -> OperationCounters {
        self.operations[kind.index()]
    }

    /// Sessions ended by one termination kind.
    pub fn process_terminated(&self, kind: TerminationKind) -> u64 {
        self.process_terminations[kind.index()]
    }
}

impl Default for MetricsSnapshot {
    fn default() -> Self {
        Self {
            creates_ok: 0,
            creates_failed: 0,
            create_cleanup_pending: 0,
            create_cleanup_completed: 0,
            operations: std::array::from_fn(|_| OperationCounters::default()),
            cleanup_attempts: 0,
            cleanup_failures: 0,
            pool_ready: 0,
            pool_leased: 0,
            pool_retired: 0,
            pool_refilled: 0,
            snapshot_captures_ok: 0,
            snapshot_captures_failed: 0,
            snapshot_restores_ok: 0,
            snapshot_restores_failed: 0,
            process_sessions_started: 0,
            process_terminations: [0; TerminationKind::COUNT],
        }
    }
}

#[derive(Debug, Default)]
struct OperationAtomics {
    ok: AtomicU64,
    error: AtomicU64,
    cancelled: AtomicU64,
    timed_out: AtomicU64,
}

#[derive(Debug)]
struct Counters {
    creates_ok: AtomicU64,
    creates_failed: AtomicU64,
    create_cleanup_pending: AtomicU64,
    create_cleanup_completed: AtomicU64,
    operations: [OperationAtomics; OperationKind::COUNT],
    cleanup_attempts: AtomicU64,
    cleanup_failures: AtomicU64,
    pool_ready: AtomicU64,
    pool_leased: AtomicU64,
    pool_retired: AtomicU64,
    pool_refilled: AtomicU64,
    snapshot_captures_ok: AtomicU64,
    snapshot_captures_failed: AtomicU64,
    snapshot_restores_ok: AtomicU64,
    snapshot_restores_failed: AtomicU64,
    process_sessions_started: AtomicU64,
    process_terminations: [AtomicU64; TerminationKind::COUNT],
}

impl Default for Counters {
    fn default() -> Self {
        Self {
            creates_ok: AtomicU64::new(0),
            creates_failed: AtomicU64::new(0),
            create_cleanup_pending: AtomicU64::new(0),
            create_cleanup_completed: AtomicU64::new(0),
            operations: std::array::from_fn(|_| OperationAtomics::default()),
            cleanup_attempts: AtomicU64::new(0),
            cleanup_failures: AtomicU64::new(0),
            pool_ready: AtomicU64::new(0),
            pool_leased: AtomicU64::new(0),
            pool_retired: AtomicU64::new(0),
            pool_refilled: AtomicU64::new(0),
            snapshot_captures_ok: AtomicU64::new(0),
            snapshot_captures_failed: AtomicU64::new(0),
            snapshot_restores_ok: AtomicU64::new(0),
            snapshot_restores_failed: AtomicU64::new(0),
            process_sessions_started: AtomicU64::new(0),
            process_terminations: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

/// Bounded, cloneable access to caller-owned lifecycle counters.
///
/// [`SandboxMetrics::new`] allocates one `Arc` holding a fixed set of atomic
/// counters; clones share that state. [`SandboxMetrics::disabled`] (also the
/// `Default`) allocates nothing and every recording method returns immediately.
/// There is no process-global registry, exporter, or label set: only the fixed
/// enums [`OperationKind`], [`OperationOutcome`], and [`TerminationKind`] are
/// accepted.
///
/// An untrusted caller cannot choose a label or add a dimension, so counter
/// memory is bounded regardless of usage. Increments saturate instead of
/// wrapping; the pending-cleanup gauge saturates at zero.
#[derive(Debug, Clone, Default)]
pub struct SandboxMetrics {
    counters: Option<Arc<Counters>>,
}

impl SandboxMetrics {
    /// Creates an enabled handle with zeroed counters.
    pub fn new() -> Self {
        Self {
            counters: Some(Arc::new(Counters::default())),
        }
    }

    /// Creates a handle that records nothing and allocates nothing.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Returns whether this handle records counters.
    pub fn is_enabled(&self) -> bool {
        self.counters.is_some()
    }

    /// Records one successful creation.
    pub fn record_create_ok(&self) {
        if let Some(counters) = &self.counters {
            increment(&counters.creates_ok);
        }
    }

    /// Records one failed creation, whether or not cleanup is still pending.
    pub fn record_create_failed(&self) {
        if let Some(counters) = &self.counters {
            increment(&counters.creates_failed);
        }
    }

    /// Records a creation whose owned cleanup is still owed.
    pub fn record_create_cleanup_pending(&self) {
        if let Some(counters) = &self.counters {
            increment(&counters.create_cleanup_pending);
        }
    }

    /// Records confirmation of cleanup after a failed creation.
    pub fn record_create_cleanup_completed(&self) {
        if let Some(counters) = &self.counters {
            saturating_decrement(&counters.create_cleanup_pending);
            increment(&counters.create_cleanup_completed);
        }
    }

    /// Records one operation outcome for a fixed operation kind.
    pub fn record_operation(&self, kind: OperationKind, outcome: OperationOutcome) {
        if let Some(counters) = &self.counters {
            let counters = &counters.operations[kind.index()];
            let counter = match outcome {
                OperationOutcome::Ok => &counters.ok,
                OperationOutcome::Error => &counters.error,
                OperationOutcome::Cancelled => &counters.cancelled,
                OperationOutcome::TimedOut => &counters.timed_out,
            };
            increment(counter);
        }
    }

    /// Records one cleanup attempt.
    pub fn record_cleanup_attempt(&self) {
        if let Some(counters) = &self.counters {
            increment(&counters.cleanup_attempts);
        }
    }

    /// Records one cleanup attempt that did not confirm full cleanup.
    pub fn record_cleanup_failure(&self) {
        if let Some(counters) = &self.counters {
            increment(&counters.cleanup_failures);
        }
    }

    /// Records a pool slot reaching `Ready`.
    pub fn record_pool_ready(&self) {
        if let Some(counters) = &self.counters {
            increment(&counters.pool_ready);
        }
    }

    /// Records a lease handoff to a caller.
    pub fn record_pool_leased(&self) {
        if let Some(counters) = &self.counters {
            increment(&counters.pool_leased);
        }
    }

    /// Records a pool slot retired for destruction.
    pub fn record_pool_retired(&self) {
        if let Some(counters) = &self.counters {
            increment(&counters.pool_retired);
        }
    }

    /// Records a pool slot recreated after initial warm-up.
    pub fn record_pool_refilled(&self) {
        if let Some(counters) = &self.counters {
            increment(&counters.pool_refilled);
        }
    }

    /// Records a workspace snapshot capture.
    pub fn record_snapshot_capture(&self, ok: bool) {
        if let Some(counters) = &self.counters {
            if ok {
                increment(&counters.snapshot_captures_ok);
            } else {
                increment(&counters.snapshot_captures_failed);
            }
        }
    }

    /// Records a workspace snapshot restore.
    pub fn record_snapshot_restore(&self, ok: bool) {
        if let Some(counters) = &self.counters {
            if ok {
                increment(&counters.snapshot_restores_ok);
            } else {
                increment(&counters.snapshot_restores_failed);
            }
        }
    }

    /// Records one started interactive process session.
    pub fn record_process_started(&self) {
        if let Some(counters) = &self.counters {
            increment(&counters.process_sessions_started);
        }
    }

    /// Records one ended interactive process session.
    pub fn record_process_ended(&self, kind: TerminationKind) {
        if let Some(counters) = &self.counters {
            increment(&counters.process_terminations[kind.index()]);
        }
    }

    /// Copies the current counters without resetting them.
    ///
    /// A disabled handle returns the all-zero default snapshot.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let Some(counters) = &self.counters else {
            return MetricsSnapshot::default();
        };
        MetricsSnapshot {
            creates_ok: load(&counters.creates_ok),
            creates_failed: load(&counters.creates_failed),
            create_cleanup_pending: load(&counters.create_cleanup_pending),
            create_cleanup_completed: load(&counters.create_cleanup_completed),
            operations: std::array::from_fn(|index| OperationCounters {
                ok: load(&counters.operations[index].ok),
                error: load(&counters.operations[index].error),
                cancelled: load(&counters.operations[index].cancelled),
                timed_out: load(&counters.operations[index].timed_out),
            }),
            cleanup_attempts: load(&counters.cleanup_attempts),
            cleanup_failures: load(&counters.cleanup_failures),
            pool_ready: load(&counters.pool_ready),
            pool_leased: load(&counters.pool_leased),
            pool_retired: load(&counters.pool_retired),
            pool_refilled: load(&counters.pool_refilled),
            snapshot_captures_ok: load(&counters.snapshot_captures_ok),
            snapshot_captures_failed: load(&counters.snapshot_captures_failed),
            snapshot_restores_ok: load(&counters.snapshot_restores_ok),
            snapshot_restores_failed: load(&counters.snapshot_restores_failed),
            process_sessions_started: load(&counters.process_sessions_started),
            process_terminations: std::array::from_fn(|index| {
                load(&counters.process_terminations[index])
            }),
        }
    }
}

#[inline]
fn increment(counter: &AtomicU64) {
    let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

#[inline]
fn saturating_decrement(counter: &AtomicU64) {
    let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_sub(1))
    });
}

#[inline]
fn load(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_fixed_counters() {
        let metrics = SandboxMetrics::new();
        metrics.record_create_ok();
        metrics.record_create_failed();
        metrics.record_create_cleanup_pending();
        metrics.record_operation(OperationKind::Exec, OperationOutcome::TimedOut);
        metrics.record_cleanup_attempt();
        metrics.record_cleanup_failure();
        metrics.record_pool_ready();
        metrics.record_pool_leased();
        metrics.record_pool_retired();
        metrics.record_pool_refilled();
        metrics.record_snapshot_capture(true);
        metrics.record_snapshot_capture(false);
        metrics.record_snapshot_restore(true);
        metrics.record_snapshot_restore(false);
        metrics.record_process_started();
        metrics.record_process_ended(TerminationKind::OutputLimit);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.creates_ok, 1);
        assert_eq!(snapshot.creates_failed, 1);
        assert_eq!(snapshot.create_cleanup_pending, 1);
        assert_eq!(snapshot.create_cleanup_completed, 0);
        assert_eq!(snapshot.operation(OperationKind::Exec).timed_out, 1);
        assert_eq!(snapshot.cleanup_attempts, 1);
        assert_eq!(snapshot.cleanup_failures, 1);
        assert_eq!(snapshot.pool_ready, 1);
        assert_eq!(snapshot.pool_leased, 1);
        assert_eq!(snapshot.pool_retired, 1);
        assert_eq!(snapshot.pool_refilled, 1);
        assert_eq!(snapshot.snapshot_captures_ok, 1);
        assert_eq!(snapshot.snapshot_captures_failed, 1);
        assert_eq!(snapshot.snapshot_restores_ok, 1);
        assert_eq!(snapshot.snapshot_restores_failed, 1);
        assert_eq!(snapshot.process_sessions_started, 1);
        assert_eq!(snapshot.process_terminated(TerminationKind::OutputLimit), 1);
        assert_eq!(snapshot.process_terminated(TerminationKind::Exited), 0);
    }

    #[test]
    fn clone_shares_counters() {
        let metrics = SandboxMetrics::new();
        metrics.record_create_ok();
        let clone = metrics.clone();
        clone.record_create_ok();
        assert_eq!(metrics.snapshot().creates_ok, 2);
        assert_eq!(clone.snapshot().creates_ok, 2);
    }

    #[test]
    fn saturates_at_u64_max_and_zero() {
        let metrics = SandboxMetrics::new();
        let counters = metrics.counters.as_ref().unwrap();
        counters.creates_ok.store(u64::MAX, Ordering::Relaxed);
        counters
            .create_cleanup_pending
            .store(u64::MAX, Ordering::Relaxed);
        counters.pool_ready.store(u64::MAX, Ordering::Relaxed);
        metrics.record_create_ok();
        metrics.record_create_cleanup_pending();
        metrics.record_pool_ready();
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.creates_ok, u64::MAX);
        assert_eq!(snapshot.create_cleanup_pending, u64::MAX);
        assert_eq!(snapshot.pool_ready, u64::MAX);
        let empty = SandboxMetrics::new();
        empty.record_create_cleanup_completed();
        let snapshot = empty.snapshot();
        assert_eq!(snapshot.create_cleanup_pending, 0);
        assert_eq!(snapshot.create_cleanup_completed, 1);
    }

    #[test]
    fn disabled_handle_is_a_no_op() {
        let metrics = SandboxMetrics::default();
        assert!(!metrics.is_enabled());
        metrics.record_create_ok();
        metrics.record_create_failed();
        metrics.record_create_cleanup_pending();
        metrics.record_create_cleanup_completed();
        metrics.record_operation(OperationKind::Start, OperationOutcome::Ok);
        metrics.record_cleanup_attempt();
        metrics.record_cleanup_failure();
        metrics.record_pool_ready();
        metrics.record_pool_leased();
        metrics.record_pool_retired();
        metrics.record_pool_refilled();
        metrics.record_snapshot_capture(true);
        metrics.record_snapshot_restore(false);
        metrics.record_process_started();
        metrics.record_process_ended(TerminationKind::Exited);
        assert_eq!(metrics.snapshot(), MetricsSnapshot::default());
        assert!(!SandboxMetrics::disabled().is_enabled());
    }
}
