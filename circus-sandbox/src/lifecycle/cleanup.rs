//! Cleanup receipts observe the lifecycle worker, never own its task.

use std::time::Duration;

use tokio::sync::{oneshot, watch};

use crate::infrastructure::Backend;
use crate::observability::events;

use super::operations::destroy_report;
use super::{CleanupReport, Ownership, SandboxError, SandboxPhase, State};

pub(super) fn ownership_label(ownership: Ownership) -> &'static str {
    match ownership {
        Ownership::ConfirmedAbsent => "confirmed_absent",
        Ownership::StillPresent => "still_present",
        Ownership::Unknown => "unknown",
    }
}

#[derive(Debug, Clone)]
pub(super) struct Progress {
    pub(super) report: CleanupReport,
    pub(super) complete: bool,
    pub(super) attempted: bool,
}

/// Observation of instance and staging cleanup by the lifecycle worker.
///
/// The lifecycle worker retains ownership of the backend instance and local
/// staging and retries cleanup independently of all receipts. Cloning or
/// dropping a receipt, or cancelling [`Self::wait`], never cancels that worker.
/// This is an in-process receipt, not a durable cleanup guarantee across
/// process or runtime shutdown.
#[derive(Debug, Clone)]
pub struct CleanupReceipt {
    progress: watch::Receiver<Progress>,
}

impl CleanupReceipt {
    /// Latest cleanup report without waiting.
    ///
    /// The initial `Unknown` report means cleanup has not been attempted; it is
    /// not itself evidence of a failed attempt. Use [`Self::wait`] for proof
    /// that both instance absence and staging removal completed.
    pub fn status(&self) -> CleanupReport {
        self.progress.borrow().report.clone()
    }

    /// Returns whether the worker has published at least one cleanup attempt.
    pub fn attempted(&self) -> bool {
        self.progress.borrow().attempted
    }

    /// Returns whether the most recently published attempt failed.
    ///
    /// Before the first attempt this is `false`. Any attempt that did not reach
    /// full completion counts as failed, including staging cleanup failure after
    /// the instance itself was confirmed absent: the worker still owns cleanup.
    pub fn last_attempt_failed(&self) -> bool {
        let progress = self.progress.borrow();
        progress.attempted && !progress.complete
    }

    /// Waits until the worker confirms instance absence and staging removal.
    ///
    /// Each call observes independently, so a cancelled wait can be retried and
    /// cloned receipts can wait concurrently. There is no implicit deadline.
    /// Returns [`SandboxError::WorkerGone`] if the worker ends without confirming
    /// full cleanup. A final confirmed result remains available after it exits.
    pub async fn wait(&self) -> Result<CleanupReport, SandboxError> {
        let mut progress = self.progress.clone();
        loop {
            {
                let current = progress.borrow_and_update();
                if current.complete {
                    return Ok(current.report.clone());
                }
            }
            progress
                .changed()
                .await
                .map_err(|_| SandboxError::WorkerGone {
                    detail: "cleanup worker ended before confirming instance and staging cleanup"
                        .to_string(),
                })?;
        }
    }
}

pub(super) fn channel(name: String) -> (watch::Sender<Progress>, CleanupReceipt) {
    let (sender, progress) = watch::channel(Progress {
        report: CleanupReport {
            instance_id: Some(name),
            ownership: Ownership::Unknown,
            detail: "creation pending; cleanup has not been attempted".to_string(),
        },
        complete: false,
        attempted: false,
    });
    (sender, CleanupReceipt { progress })
}

pub(super) async fn failed_creation<B: Backend>(
    state: &mut State<B>,
    ready: oneshot::Sender<Result<(), SandboxError>>,
    source: SandboxError,
    initial_report: Option<CleanupReport>,
) {
    state.creation_cleanup = true;
    state.metrics.record_create_cleanup_pending();
    // Preserve the identity even if this first attempt completes cleanup.
    let instance_id = state
        .id
        .as_ref()
        .map_or_else(|| state.name.clone(), |id| id.as_str().to_string());
    if initial_report.is_none() {
        destroy_report(state).await;
    }
    let _ = ready.send(Err(SandboxError::CreationCleanupRequired {
        instance_id,
        source: Box::new(source),
        cleanup: CleanupReceipt {
            progress: state.cleanup.subscribe(),
        },
    }));
    // Neither a dropped ready receiver nor the last receipt stops this loop.
    retry_cleanup(state, true, true).await;
}

pub(super) async fn cleanup_until_done<B: Backend>(state: &mut State<B>) {
    retry_cleanup(state, false, false).await;
}

async fn retry_cleanup<B: Backend>(
    state: &mut State<B>,
    mut attempted: bool,
    creation_cleanup: bool,
) {
    let mut attempts: u32 = u32::from(attempted);
    let mut delay = Duration::from_millis(100);
    while state.phase != SandboxPhase::Destroyed {
        if attempted {
            events::cleanup_retry(
                attempts.saturating_add(1),
                u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                creation_cleanup,
            );
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(30));
        }
        destroy_report(state).await;
        attempts = attempts.saturating_add(1);
        attempted = true;
    }
    if creation_cleanup {
        state.metrics.record_create_cleanup_completed();
        events::creation_cleanup_completed(attempts);
    }
}
