//! Fixed-revision Git preparation, private worktrees, and code-only workspace import.
mod owner;
#[cfg(test)]
mod owner_tests;
mod worktree;
#[cfg(test)]
mod worktree_tests;
use super::{CodeCopyError, SnapshotBudget, Workspace};
use crate::runtime::ProcessError;
use std::{path::PathBuf, time::Duration};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("invalid git options: {0}")]
    Options(String),
    #[error("invalid source: {0}")]
    Source(String),
    #[error("invalid revision: {0}")]
    Revision(String),
    #[error("git process failed: {0}")]
    Process(#[from] ProcessError),
    #[error("git exited unsuccessfully ({status})")]
    Command { status: std::process::ExitStatus },
    #[error("code copy failed: {0}")]
    Copy(#[from] CodeCopyError),
    #[error("cleanup failed: {0}")]
    Cleanup(#[source] std::io::Error),
    #[error("Git cleanup remains owned by its worker: {source}")]
    CleanupRequired {
        #[source]
        source: Box<GitError>,
        cleanup: GitCleanup,
    },
    #[error("Git owner exited without confirming cleanup")]
    WorkerGone,
    #[error("Git handle is closed")]
    Closed,
}

/// Opaque receipt. Dropping it never cancels the independent cleanup owner.
#[derive(Clone, Debug)]
pub struct GitCleanup {
    done: watch::Receiver<bool>,
}
impl GitCleanup {
    /// Wait for both process termination and all owned directories to be removed.
    pub async fn wait(&self) -> Result<(), GitError> {
        let mut done = self.done.clone();
        loop {
            if *done.borrow_and_update() {
                return Ok(());
            }
            done.changed().await.map_err(|_| GitError::WorkerGone)?;
        }
    }
}

#[derive(Clone)]
pub struct GitOptions {
    executable: PathBuf,
    storage_root: PathBuf,
    timeout: Duration,
    max_output_bytes: usize,
}
impl std::fmt::Debug for GitOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitOptions")
            .field("timeout", &self.timeout)
            .field("max_output_bytes", &self.max_output_bytes)
            .finish_non_exhaustive()
    }
}
impl GitOptions {
    pub fn new(
        executable: PathBuf,
        storage_root: PathBuf,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Result<Self, GitError> {
        if !executable.is_absolute() {
            return Err(GitError::Options("git executable must be absolute".into()));
        }
        if !storage_root.is_absolute() {
            return Err(GitError::Options("storage root must be absolute".into()));
        }
        if timeout.is_zero() || max_output_bytes == 0 {
            return Err(GitError::Options(
                "timeout and output budget must be positive".into(),
            ));
        }
        Ok(Self {
            executable,
            storage_root,
            timeout,
            max_output_bytes,
        })
    }
}

pub struct GitSource(SourceKind);
enum SourceKind {
    Local(PathBuf),
    Https(Url),
}
impl std::fmt::Debug for GitSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitSource").finish_non_exhaustive()
    }
}
impl GitSource {
    /// Grants access to a trusted filesystem repository, not filesystem confinement.
    pub fn local(path: PathBuf) -> Result<Self, GitError> {
        if !path.is_absolute() {
            return Err(GitError::Source("local source must be absolute".into()));
        }
        Ok(Self(SourceKind::Local(path)))
    }
    /// Grants HTTPS access; this is not an SSRF or network isolation policy.
    pub fn https(url: Url) -> Result<Self, GitError> {
        if url.scheme() != "https"
            || url.cannot_be_a_base()
            || url.username() != ""
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(GitError::Source(
                "URL must be HTTPS without credentials, query, or fragment".into(),
            ));
        }
        Ok(Self(SourceKind::Https(url)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitRevision(String);
impl GitRevision {
    pub fn parse(value: &str) -> Result<Self, GitError> {
        if (value.len() != 40 && value.len() != 64) || !value.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(GitError::Revision(
                "expected a full hexadecimal commit".into(),
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }
}

/// A message handle; only the detached worker owns host resources. Each
/// worktree created through this handle has its own independent owner.
pub struct GitCheckout {
    requests: mpsc::UnboundedSender<owner::Request>,
    shutdown: CancellationToken,
    revision: GitRevision,
}
impl Drop for GitCheckout {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}
impl std::fmt::Debug for GitCheckout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitCheckout")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}
impl GitCheckout {
    /// Prepare and verify an exact commit. Cancellation retains an independent
    /// owner until started processes and blocking stages have drained and cleaned.
    pub async fn clone(
        source: GitSource,
        revision: GitRevision,
        options: GitOptions,
    ) -> Result<Self, GitError> {
        let shutdown = CancellationToken::new();
        let guard = shutdown.clone().drop_guard();
        let (requests, receive) = mpsc::unbounded_channel();
        let (ready, result) = oneshot::channel();
        tokio::spawn(owner::serve(
            source,
            revision.clone(),
            options,
            shutdown.clone(),
            receive,
            ready,
        ));
        result.await.map_err(|_| GitError::WorkerGone)??;
        guard.disarm();
        Ok(Self {
            requests,
            shutdown,
            revision,
        })
    }
    pub fn revision(&self) -> &GitRevision {
        &self.revision
    }
    /// Copy only repository code. The deadline includes queued and blocking work;
    /// cancellation drains a started copy and explicitly removes its destination.
    pub async fn copy_to_workspace(
        &self,
        max_file_bytes: usize,
        budget: SnapshotBudget,
    ) -> Result<Workspace, GitError> {
        let cancel = self.shutdown.child_token();
        let _guard = cancel.clone().drop_guard();
        let (reply, result) = oneshot::channel();
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (accepted, acknowledgment) = oneshot::channel();
        self.requests
            .send(owner::Request::Copy {
                max_file_bytes,
                budget,
                cancel,
                started: tokio::time::Instant::now(),
                reply,
                slot: slot.clone(),
                acknowledgment,
            })
            .map_err(|_| GitError::Closed)?;
        result.await.map_err(|_| GitError::WorkerGone)??;
        let result = slot
            .lock()
            .expect("Git delivery lock poisoned")
            .take()
            .ok_or(GitError::WorkerGone)?;
        let _ = accepted.send(());
        result
    }
    /// Creates one detached worktree of the prepared repository at the verified
    /// fixed commit. The checkout keeps ownership of the shared private
    /// repository; the returned worktree owns its private directory and Git
    /// administrative entry.
    ///
    /// Uses the checkout's configured [`GitOptions`] timeout as this creation's
    /// whole-operation deadline. Concurrent worktrees are independent: removing
    /// one never removes the checkout, the repository, or another worktree.
    /// Creation fails with [`GitError::Closed`] after checkout cleanup. Dropping
    /// the returned handle cancels its owner, which drains started blocking work,
    /// removes the worktree, and keeps retrying removal it cannot confirm.
    pub async fn create_worktree(&self) -> Result<GitWorktree, GitError> {
        let (reply, result) = oneshot::channel();
        self.requests
            .send(owner::Request::Worktree {
                started: tokio::time::Instant::now(),
                reply,
            })
            .map_err(|_| GitError::Closed)?;
        result.await.map_err(|_| GitError::WorkerGone)?
    }
    /// Idempotent explicit cleanup. A failed removal retains its identity in the
    /// worker and returns a receipt; retries never consume a failed TempDir.
    pub async fn cleanup(&mut self) -> Result<(), GitError> {
        let (reply, result) = oneshot::channel();
        self.requests
            .send(owner::Request::Cleanup(reply))
            .map_err(|_| GitError::Closed)?;
        result.await.map_err(|_| GitError::WorkerGone)?
    }
}

/// One private detached worktree of a [`GitCheckout`]'s prepared repository.
///
/// The checkout owns the shared private repository and the primary checkout
/// directory. This value owns exactly one private worktree directory and the
/// administrative entry Git records for it, plus any copies it produces. Its
/// detached owner serves code-only copies and confirmed removal; dropping the
/// handle cancels that owner but does not drop its cleanup responsibility.
///
/// Cleanup uses `git worktree remove --force` for this worktree only, then
/// `git worktree prune --expire now`, then removes this private directory. A dirty worktree,
/// including modified or untracked files, is therefore removed without
/// preserving those changes; cleanup never removes the source repository, the
/// checkout's primary directory, or another worktree. Removal is retried in a
/// retained owner when Git, process termination, or directory deletion cannot
/// be confirmed. This is trusted host-side Git, not an OS process or filesystem
/// isolation boundary, and it enforces no disk quota.
pub struct GitWorktree {
    requests: mpsc::UnboundedSender<worktree::Request>,
    shutdown: CancellationToken,
    revision: GitRevision,
    /// Used by dirty-worktree tests; the private path is not part of the API.
    #[cfg_attr(not(test), allow(dead_code))]
    tree: PathBuf,
}
impl Drop for GitWorktree {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}
impl std::fmt::Debug for GitWorktree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitWorktree")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}
impl GitWorktree {
    /// The fixed commit checked out in this worktree.
    pub fn revision(&self) -> &GitRevision {
        &self.revision
    }
    /// Copies only worktree code into a fresh [`Workspace`]. The worktree's
    /// `.git` file is excluded, as are its private administrative metadata.
    ///
    /// `max_file_bytes` and `budget` have [`Workspace::new`] and
    /// [`SnapshotBudget`] semantics. Symlinks and non-regular nodes are rejected
    /// by the same filtered copy used by [`GitCheckout::copy_to_workspace`].
    /// Each call is independent and starts its own whole-operation deadline of
    /// the checkout's configured [`GitOptions`] timeout, covering queueing and
    /// the blocking copy. Cancellation or timeout then drains the started
    /// blocking copy and removes its destination outside that deadline. This
    /// method blocks file work on a blocking worker, never on the async runtime.
    ///
    /// Dropping this future cancels the request; the owner drains the started
    /// blocking copy and explicitly removes its destination. If that removal
    /// cannot be confirmed, the owner retains the destination and keeps
    /// retrying in the background, stops serving further copies, and this call
    /// fails with a receipt. [`Self::cleanup`] remains available while that
    /// retained removal is pending.
    pub async fn copy_to_workspace(
        &self,
        max_file_bytes: usize,
        budget: SnapshotBudget,
    ) -> Result<Workspace, GitError> {
        let cancel = self.shutdown.child_token();
        let _guard = cancel.clone().drop_guard();
        let (reply, result) = oneshot::channel();
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (accepted, acknowledgment) = oneshot::channel();
        self.requests
            .send(worktree::Request::Copy {
                max_file_bytes,
                budget,
                cancel,
                started: tokio::time::Instant::now(),
                reply,
                slot: slot.clone(),
                acknowledgment,
            })
            .map_err(|_| GitError::Closed)?;
        result.await.map_err(|_| GitError::WorkerGone)??;
        let result = slot
            .lock()
            .expect("Git delivery lock poisoned")
            .take()
            .ok_or(GitError::WorkerGone)?;
        let _ = accepted.send(());
        result
    }
    /// Explicit, idempotent removal of this worktree and its private directory.
    ///
    /// Returns `Ok(())` when already removed. A removal that cannot be confirmed
    /// returns [`GitError::CleanupRequired`] with an opaque [`GitCleanup`]
    /// receipt; the retained owner continues retrying independently of the
    /// receipt. Copies after successful cleanup return
    /// [`GitError::Closed`].
    pub async fn cleanup(&mut self) -> Result<(), GitError> {
        let (reply, result) = oneshot::channel();
        self.requests
            .send(worktree::Request::Cleanup(reply))
            .map_err(|_| GitError::Closed)?;
        result.await.map_err(|_| GitError::WorkerGone)?
    }
}
