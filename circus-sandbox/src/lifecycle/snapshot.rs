//! Serialized workspace-only capture and staging replacement.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::infrastructure::{
    Backend, EngineError, EngineErrorKind, WorkspaceEntry, WorkspaceEntryKind,
};
use crate::workspace::{
    CodeSnapshot, SnapshotBody, SnapshotBudget, SnapshotEntry, SnapshotError, WorkspaceError,
};

use super::operations::{
    cleanup_engine_error, destroy_report, need, pause, phase_err, require_capability, required_id,
    resume, spawn_block,
};
use super::{Op, Sandbox, SandboxError, SandboxOperation, SandboxPhase, State};

impl Sandbox {
    /// Captures regular workspace files and directories, not processes or memory.
    ///
    /// In `Created`, captures host staging. In `Running`, the lifecycle worker
    /// enumerates the configured workspace through the confined guest helper,
    /// checks the budgets, and reads each file. A user-`Paused` Docker or
    /// containerd guest is briefly resumed and paused again (also on an
    /// ordinary validation or budget failure); Kubernetes Pods have no pause
    /// state and are read live. Other phases return
    /// [`SandboxError::InvalidPhase`].
    ///
    /// `budget` bounds retained file payload and entries. `max_archive_bytes`
    /// must be positive and separately bounds the transport encoding of the
    /// manifest that enumerates the workspace; a zero bound fails before
    /// queueing. Payload bytes are bounded separately: the configured per-file
    /// limit caps every read and `budget` caps the retained total. All
    /// operations are serialized with other sandbox mutations. The configured
    /// file timeout covers pause, enumeration, payload reads and resume
    /// together. Cancellation or timeout uses whole-instance cleanup; failed
    /// cleanup retains ownership. Backend pause, transport or resume failures
    /// also trigger owned cleanup. Domain failures — unsafe guest nodes,
    /// non-portable names, and budget violations — are matchable as
    /// [`SandboxError::Snapshot`] and leave a resumed sandbox usable.
    /// A deadline initiates cancellation, not a bound on cleanup completion.
    /// [`SandboxError::SnapshotInterrupted`] preserves both an interruption
    /// and any snapshot failure, including owned temporary cleanup failures.
    pub async fn capture_workspace_snapshot(
        &self,
        budget: SnapshotBudget,
        max_archive_bytes: u64,
    ) -> Result<Arc<CodeSnapshot>, SandboxError> {
        if max_archive_bytes == 0 {
            return Err(SnapshotError::InvalidArchiveLimit.into());
        }
        self.call(|reply| Op::CaptureSnapshot {
            budget,
            max_manifest_bytes: max_archive_bytes,
            reply,
        })
        .await?
    }

    /// Replaces `Created` staging with this snapshot for the next `start`.
    ///
    /// Running, paused, destroyed and cleanup-required sandboxes reject this
    /// operation without modification. The target's configured per-file and
    /// workspace payload bounds apply, not the source snapshot's limits. The
    /// queued payload shares the supplied `Arc`; it is not deep-cloned.
    ///
    /// Validation and creation happen before old staging is removed. If old
    /// staging cleanup fails, the old workspace remains installed (its cleanup
    /// may have partially removed files), the replacement is explicitly cleaned,
    /// and [`SandboxError::Snapshot`] reports the failure. If replacement
    /// cleanup also fails, the nested [`SnapshotError::RestoreCleanup`] owns it
    /// for retry. Successful replacement commits only after old cleanup succeeds.
    /// Uses the configured file timeout and serialized worker; cancellation or
    /// timeout cleans the whole owned instance and staging, retaining ownership
    /// when cleanup cannot be confirmed. Already-started filesystem work is
    /// drained before cleanup; completion may exceed the deadline. Staging
    /// installation is worker-owned and never occurs after interruption.
    pub async fn restore_workspace_snapshot(
        &self,
        snapshot: Arc<CodeSnapshot>,
    ) -> Result<(), SandboxError> {
        self.call(|reply| Op::RestoreSnapshot { snapshot, reply })
            .await?
    }
}

struct Deadline {
    cancel: CancellationToken,
    shutdown: CancellationToken,
    at: tokio::time::Instant,
}

impl Deadline {
    fn new<B: Backend>(state: &State<B>, cancel: &CancellationToken) -> Self {
        Self {
            cancel: cancel.clone(),
            shutdown: state.shutdown.clone(),
            at: tokio::time::Instant::now() + state.config.file_timeout(),
        }
    }

    async fn network<T>(&self, future: impl Future<Output = T>) -> Result<T, bool> {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => Err(true),
            () = self.shutdown.cancelled() => Err(true),
            () = tokio::time::sleep_until(self.at) => Err(false),
            value = future => Ok(value),
        }
    }

    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> (Result<T, SandboxError>, Option<bool>) {
        let mut task = tokio::task::spawn_blocking(work);
        let (result, interrupted) = match self.network(&mut task).await {
            Ok(result) => (result, None),
            Err(cancelled) => (task.await, Some(cancelled)),
        };
        (
            result.map_err(|err| SandboxError::WorkerGone {
                detail: err.to_string(),
            }),
            interrupted,
        )
    }
}

async fn interrupted<B: Backend, T>(
    state: &mut State<B>,
    cancelled: bool,
    operation: SandboxOperation,
    snapshot: Option<SnapshotError>,
) -> Result<T, SandboxError> {
    let report = destroy_report(state).await;
    let interruption = if cancelled {
        SandboxError::Cancelled {
            instance_id: report.instance_id,
            ownership: report.ownership,
            operation,
            detail: report.detail,
        }
    } else {
        SandboxError::TimedOut {
            instance_id: report.instance_id,
            ownership: report.ownership,
            operation,
            detail: report.detail,
        }
    };
    Err(match snapshot {
        Some(snapshot) => SandboxError::SnapshotInterrupted {
            interruption: Box::new(interruption),
            snapshot: Box::new(snapshot),
        },
        None => interruption,
    })
}

pub(super) async fn capture<B: Backend>(
    state: &mut State<B>,
    budget: SnapshotBudget,
    max_manifest_bytes: u64,
    cancel: &CancellationToken,
) -> Result<Arc<CodeSnapshot>, SandboxError> {
    require_capability(
        state.backend.capabilities().workspace_snapshot,
        "capture a workspace snapshot",
    )?;
    let deadline = Deadline::new(state, cancel);
    if state.phase == SandboxPhase::Created {
        let workspace = Arc::clone(&state.workspace);
        let (result, stopped) = deadline
            .blocking(move || {
                workspace
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .capture_code_snapshot(budget)
            })
            .await;
        if let Some(cancelled) = stopped {
            return interrupted(
                state,
                cancelled,
                SandboxOperation::CaptureWorkspaceSnapshot,
                result.ok().and_then(Result::err),
            )
            .await;
        }
        return Ok(Arc::new(result??));
    }
    if !matches!(state.phase, SandboxPhase::Running | SandboxPhase::Paused) {
        return Err(phase_err(state, "capture_workspace_snapshot"));
    }
    // Enumeration uses the backend's guest exec transport. Docker rejects exec
    // while a container is paused, so a running source is read live (the
    // manifest and per-file size checks still fail closed on concurrent
    // change); a user-paused source is briefly resumed and paused again.
    // Kubernetes has no pause state, so a running Pod is read directly.
    let restore_pause = state.phase == SandboxPhase::Paused;
    if restore_pause {
        match deadline.network(resume(state)).await {
            Ok(result) => result?,
            Err(cancelled) => {
                return interrupted(
                    state,
                    cancelled,
                    SandboxOperation::CaptureWorkspaceSnapshot,
                    None,
                )
                .await;
            }
        }
    }
    let id = required_id(state)?;
    let path = state.config.isolation().workspace_path().to_owned();
    let user = state.user.clone();
    let max_file = state.config.limits().max_file_bytes();
    let entries = match deadline
        .network(state.backend.list_workspace(
            &id,
            &user,
            &path,
            max_manifest_bytes,
            budget.max_entries(),
        ))
        .await
    {
        Ok(Ok(entries)) => entries,
        Ok(Err(err)) => {
            return match manifest_rejection(&err, budget) {
                Some(snapshot) => {
                    settle_capture(state, restore_pause, &deadline, Err(snapshot)).await
                }
                None => cleanup_engine_error(state, err).await,
            };
        }
        Err(cancelled) => {
            return interrupted(
                state,
                cancelled,
                SandboxOperation::CaptureWorkspaceSnapshot,
                None,
            )
            .await;
        }
    };
    // Declared metadata is checked before any payload byte is transferred, in
    // this order: entry count, per-file cap, aggregate bytes.
    if let Err(snapshot) = check_budgets(&entries, max_file, budget) {
        return settle_capture(state, restore_pause, &deadline, Err(snapshot)).await;
    }
    let mut captured = Vec::new();
    for entry in entries {
        let captured_path = PathBuf::from(entry.path());
        let body = match entry.kind() {
            WorkspaceEntryKind::Directory => SnapshotBody::Directory,
            WorkspaceEntryKind::File => {
                let bytes = match deadline
                    .network(
                        state
                            .backend
                            .download_file(&id, &user, &path, entry.path(), max_file),
                    )
                    .await
                {
                    Ok(Ok(bytes)) => bytes,
                    Ok(Err(err)) => {
                        return match payload_rejection(&err, max_file) {
                            Some(snapshot) => {
                                settle_capture(state, restore_pause, &deadline, Err(snapshot)).await
                            }
                            None => cleanup_engine_error(state, err).await,
                        };
                    }
                    Err(cancelled) => {
                        return interrupted(
                            state,
                            cancelled,
                            SandboxOperation::CaptureWorkspaceSnapshot,
                            None,
                        )
                        .await;
                    }
                };
                if u64::try_from(bytes.len()).ok() != Some(entry.size()) {
                    let snapshot = SnapshotError::InvalidArchive {
                        reason: format!(
                            "guest file {} did not match its listed size",
                            entry.path()
                        ),
                    };
                    return settle_capture(state, restore_pause, &deadline, Err(snapshot)).await;
                }
                SnapshotBody::File(bytes)
            }
        };
        captured.push(SnapshotEntry {
            path: captured_path,
            body,
        });
    }
    let snapshot = CodeSnapshot::from_entries(max_file, captured);
    settle_capture(state, restore_pause, &deadline, snapshot)
        .await
        .map(Arc::new)
}

/// Restores a user-paused source to paused, then reports `result`.
///
/// A domain failure never destroys the instance: the workspace is readable but
/// not snapshottable. A re-pause failure keeps both the failure and the cause
/// visible, as the previous capture path did.
async fn settle_capture<B: Backend, T>(
    state: &mut State<B>,
    restore_pause: bool,
    deadline: &Deadline,
    result: Result<T, SnapshotError>,
) -> Result<T, SandboxError> {
    if restore_pause {
        match deadline.network(pause(state)).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                return match result {
                    Err(snapshot) => Err(SandboxError::SnapshotInterrupted {
                        interruption: Box::new(err),
                        snapshot: Box::new(snapshot),
                    }),
                    Ok(_) => Err(err),
                };
            }
            Err(cancelled) => {
                return interrupted(
                    state,
                    cancelled,
                    SandboxOperation::CaptureWorkspaceSnapshot,
                    result.err(),
                )
                .await;
            }
        }
    }
    result.map_err(SandboxError::Snapshot)
}

/// Checks declared manifest metadata against the capture budgets.
///
/// Entry count is checked first, then each file's size against the workspace
/// per-file cap, then the running total against the capture budget. Nothing is
/// read, so an over-budget workspace never transfers its payload.
fn check_budgets(
    entries: &[WorkspaceEntry],
    max_file: usize,
    budget: SnapshotBudget,
) -> Result<(), SnapshotError> {
    let mut count = 0usize;
    let mut total = 0usize;
    for entry in entries {
        count = count.saturating_add(1);
        if count > budget.max_entries() {
            return Err(SnapshotError::EntryLimit {
                limit: budget.max_entries(),
                actual: count,
            });
        }
        if entry.kind() != WorkspaceEntryKind::File {
            continue;
        }
        let size = usize::try_from(entry.size()).unwrap_or(usize::MAX);
        if size > max_file {
            return Err(SnapshotError::Workspace(WorkspaceError::LimitExceeded {
                limit: max_file,
                actual: size,
            }));
        }
        total = total
            .checked_add(size)
            .ok_or_else(|| SnapshotError::ByteLimit {
                limit: budget.max_total_bytes(),
                actual: usize::MAX,
            })?;
        if total > budget.max_total_bytes() {
            return Err(SnapshotError::ByteLimit {
                limit: budget.max_total_bytes(),
                actual: total,
            });
        }
    }
    Ok(())
}

/// Guest listing exits that describe workspace content rather than transport.
///
/// These are snapshot-domain failures: an unsafe node, an unportable name, or
/// an entry count beyond the caller's bound. The source is resumed and stays
/// owned. The manifest byte bound and engine I/O failures keep the whole-
/// instance cleanup path because they are transport failures.
fn manifest_rejection(err: &EngineError, budget: SnapshotBudget) -> Option<SnapshotError> {
    match err.kind {
        EngineErrorKind::EntryLimit => Some(SnapshotError::EntryLimit {
            limit: budget.max_entries(),
            // The helper stops at the first entry beyond the bound.
            actual: budget.max_entries().saturating_add(1),
        }),
        EngineErrorKind::UnsafeFile | EngineErrorKind::InvalidPath | EngineErrorKind::Protocol => {
            Some(SnapshotError::InvalidArchive {
                reason: err.message.clone(),
            })
        }
        EngineErrorKind::Limit
        | EngineErrorKind::FileIo
        | EngineErrorKind::NotFound
        | EngineErrorKind::PermissionDenied
        | EngineErrorKind::Other => None,
    }
}

/// Guest payload reads that describe workspace content rather than transport.
///
/// A payload that outgrew the per-file cap is reported with the same typed
/// file-limit shape as an ordinary bounded read; an unreadable or replaced node
/// is a capture-domain failure. Both resume the paused source.
fn payload_rejection(err: &EngineError, max_file: usize) -> Option<SnapshotError> {
    match err.kind {
        EngineErrorKind::Limit => Some(SnapshotError::Workspace(WorkspaceError::LimitExceeded {
            limit: max_file,
            actual: max_file.saturating_add(1),
        })),
        EngineErrorKind::UnsafeFile
        | EngineErrorKind::InvalidPath
        | EngineErrorKind::Protocol
        | EngineErrorKind::NotFound
        | EngineErrorKind::PermissionDenied
        | EngineErrorKind::FileIo => Some(SnapshotError::InvalidArchive {
            reason: err.message.clone(),
        }),
        EngineErrorKind::EntryLimit | EngineErrorKind::Other => None,
    }
}

pub(super) async fn restore<B: Backend>(
    state: &mut State<B>,
    snapshot: Arc<CodeSnapshot>,
    cancel: &CancellationToken,
) -> Result<(), SandboxError> {
    require_capability(
        state.backend.capabilities().workspace_snapshot,
        "restore a workspace snapshot",
    )?;
    need(state, SandboxPhase::Created, "restore_workspace_snapshot")?;
    let deadline = Deadline::new(state, cancel);
    let max_file = state.config.limits().max_file_bytes();
    let max_total = usize::try_from(state.config.limits().workspace_bytes()).unwrap_or(usize::MAX);
    let (result, stopped) = deadline
        .blocking(move || snapshot.restore_with_limits(max_file, max_total))
        .await;
    if let Some(cancelled) = stopped {
        let error = match result {
            Ok(Err(err)) => Some(err),
            Ok(Ok(replacement)) => cleanup_replacement(replacement, None).await,
            Err(_) => None,
        };
        return interrupted(
            state,
            cancelled,
            SandboxOperation::RestoreWorkspaceSnapshot,
            error,
        )
        .await;
    }
    let replacement = result??;
    let workspace = Arc::clone(&state.workspace);
    let (cleaned, stopped) = deadline
        .blocking(move || {
            workspace
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .cleanup()
        })
        .await;
    if let Some(cancelled) = stopped {
        let cause = cleaned
            .ok()
            .and_then(Result::err)
            .map(SnapshotError::Workspace);
        let error = cleanup_replacement(replacement, cause).await;
        return interrupted(
            state,
            cancelled,
            SandboxOperation::RestoreWorkspaceSnapshot,
            error,
        )
        .await;
    }
    match cleaned {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            let error = cleanup_replacement(replacement, Some(err.into())).await;
            return Err(error.expect("cleanup cause is retained").into());
        }
        Err(error) => {
            return match cleanup_replacement(replacement, None).await {
                Some(snapshot) => Err(SandboxError::SnapshotInterrupted {
                    interruption: Box::new(error),
                    snapshot: Box::new(snapshot),
                }),
                None => Err(error),
            };
        }
    }
    // Only the lifecycle worker installs staging, never a detached blocking task.
    *state
        .workspace
        .lock()
        .unwrap_or_else(|err| err.into_inner()) = replacement;
    Ok(())
}

async fn cleanup_replacement(
    mut replacement: crate::workspace::Workspace,
    cause: Option<SnapshotError>,
) -> Option<SnapshotError> {
    match spawn_block(move || match replacement.cleanup() {
        Ok(()) => cause,
        Err(cleanup) => Some(SnapshotError::RestoreCleanup {
            restore: Box::new(cause.unwrap_or(SnapshotError::Interrupted)),
            destination: Box::new(replacement),
            cleanup,
        }),
    })
    .await
    {
        Ok(error) => error,
        // A panicking filesystem task has no recoverable workspace value.
        Err(err) => Some(SnapshotError::InvalidArchive {
            reason: err.to_string(),
        }),
    }
}
