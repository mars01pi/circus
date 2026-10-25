//! Private Git worktrees created from a checkout's prepared repository.
//!
//! The checkout owns the shared private repository. A worktree owns one private
//! directory plus the administrative entry Git records for it. Its detached
//! owner serves code-only copies and confirmed removal, and keeps retrying
//! removal rather than dropping an unconfirmed directory.

use super::*;
use crate::runtime::ProcessCleanup;
use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
};
use tokio::time::Instant;

pub(super) struct Context {
    checkout_root: PathBuf,
    options: GitOptions,
}

pub(super) struct Resources {
    root: Option<PathBuf>,
    tree: PathBuf,
    destinations: Vec<Workspace>,
    git_removed: bool,
}

pub(super) enum Request {
    Copy {
        max_file_bytes: usize,
        budget: SnapshotBudget,
        cancel: CancellationToken,
        started: Instant,
        reply: oneshot::Sender<Result<(), GitError>>,
        slot: std::sync::Arc<std::sync::Mutex<Option<Result<Workspace, GitError>>>>,
        acknowledgment: oneshot::Receiver<()>,
    },
    Cleanup(oneshot::Sender<Result<(), GitError>>),
}

/// Allocates the private directory outside the checkout root and adds a detached
/// worktree at exactly `revision`. The returned handle owns the directory.
pub(super) async fn create(
    checkout_root: PathBuf,
    options: GitOptions,
    revision: GitRevision,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<GitWorktree, GitError> {
    super::owner::remaining(deadline, cancel)?;
    let context = Context {
        checkout_root,
        options,
    };
    let storage = context.options.storage_root.clone();
    let (mut allocated, allocation) =
        tokio::task::spawn_blocking(move || match allocate(storage) {
            Ok((root, tree)) => (Some((root, tree)), Ok(())),
            Err(error) => (None, Err(error)),
        })
        .await
        .expect("Git worktree allocation worker panicked");
    let (root, tree) = match allocation {
        Ok(()) => allocated.take().expect("allocated worktree directory"),
        Err(error) => return Err(GitError::Cleanup(error)),
    };
    let resources = Resources {
        root: Some(root),
        tree: tree.clone(),
        destinations: Vec::new(),
        git_removed: false,
    };
    let added = async {
        let timeout = super::owner::remaining(deadline, cancel)?;
        let args = vec![
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("--detach"),
            OsString::from("--quiet"),
            resources.tree.clone().into_os_string(),
            OsString::from(revision.0.as_str()),
        ];
        super::owner::run(
            &context.checkout_root,
            args,
            None,
            &context.options,
            timeout,
            context.options.max_output_bytes,
            cancel,
        )
        .await
        .map(|_| ())
    }
    .await;
    match added {
        Ok(()) => {
            let (done, receive) = watch::channel(false);
            let cleanup = GitCleanup { done: receive };
            let shutdown = CancellationToken::new();
            let (requests, receive) = mpsc::unbounded_channel();
            tokio::spawn(serve(
                context,
                resources,
                shutdown.clone(),
                receive,
                cleanup,
                done,
            ));
            Ok(GitWorktree {
                requests,
                shutdown,
                revision,
                tree,
            })
        }
        Err(error) => Err(abandon(context, resources, error).await),
    }
}

fn allocate(storage: PathBuf) -> io::Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(&storage)?;
    let temp = tempfile::Builder::new()
        .prefix("circus-worktree-")
        .tempdir_in(storage)?;
    let root = temp.keep();
    let name = root
        .file_name()
        .ok_or_else(|| io::Error::other("worktree root has no name"))?;
    // Git derives the administrative name from the worktree's last component.
    // The random directory name keeps concurrent worktrees from colliding, and
    // the nested path does not exist yet, so `git worktree add` creates it.
    Ok((root.clone(), root.join(name)))
}

/// A failed creation is cleaned either immediately or by a retained owner that
/// keeps retrying. The receipt reports confirmation only after every step.
async fn abandon(context: Context, resources: Resources, error: GitError) -> GitError {
    let process = super::owner::pending_process(&error);
    let (resources, removal) = remove_all(&context, resources).await;
    let process = process.or_else(|| {
        removal
            .as_ref()
            .err()
            .and_then(super::owner::pending_process)
    });
    if process.is_none() && removal.is_ok() {
        return error;
    }
    let (done, receive) = watch::channel(false);
    let cleanup = GitCleanup { done: receive };
    tokio::spawn(finish(context, resources, process, done));
    super::owner::required(error, &cleanup)
}

async fn serve(
    context: Context,
    mut resources: Resources,
    shutdown: CancellationToken,
    mut requests: mpsc::UnboundedReceiver<Request>,
    cleanup: GitCleanup,
    done: watch::Sender<bool>,
) {
    loop {
        let request = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            request = requests.recv() => match request { Some(request) => request, None => break },
        };
        match request {
            Request::Cleanup(reply) => {
                let (next, result) = remove_all(&context, resources).await;
                resources = next;
                match result {
                    Ok(()) => {
                        done.send_replace(true);
                        let _ = reply.send(Ok(()));
                    }
                    Err(error) => {
                        let process = super::owner::pending_process(&error);
                        let _ = reply.send(Err(super::owner::required(error, &cleanup)));
                        finish(context, resources, process, done).await;
                        // Removal is now owned by the background retry. Preserve
                        // idempotent cleanup replies and reject further copies.
                        while let Some(request) = requests.recv().await {
                            match request {
                                Request::Cleanup(reply) => {
                                    let _ = reply.send(Ok(()));
                                }
                                Request::Copy { reply, .. } => {
                                    let _ = reply.send(Err(GitError::Closed));
                                }
                            }
                        }
                        return;
                    }
                }
            }
            Request::Copy {
                max_file_bytes,
                budget,
                cancel,
                started,
                reply,
                slot,
                acknowledgment,
            } => {
                if resources.root.is_none() {
                    let _ = reply.send(Err(GitError::Closed));
                    continue;
                }
                let deadline = started + context.options.timeout;
                if let Err(error) = super::owner::remaining(deadline, &cancel) {
                    let _ = reply.send(Err(error));
                    continue;
                }
                let tree = resources.tree.clone();
                let mut task = tokio::task::spawn_blocking(move || {
                    super::super::CodeSource::open(tree)?
                        .copy_to_workspace_excluding_git(max_file_bytes, budget)
                });
                let (result, trigger) = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => (task.await, Some(GitError::Process(ProcessError::Cancelled))),
                    _ = tokio::time::sleep_until(deadline) => (task.await, Some(GitError::Process(ProcessError::Timeout))),
                    result = &mut task => (result, None),
                };
                let result = result
                    .expect("Git worktree copy worker panicked")
                    .map_err(GitError::Copy);
                if let Some(trigger) = trigger {
                    super::owner::retain_result(result, &mut resources.destinations);
                    // Cancellation keeps this worktree only when a destination
                    // cannot be removed; the retained owner keeps retrying.
                    let destinations = std::mem::take(&mut resources.destinations);
                    let (mut partial, removal) = remove_destinations(destinations).await;
                    if removal.is_err() {
                        resources.destinations.append(&mut partial);
                        let _ = reply.send(Err(super::owner::required(trigger, &cleanup)));
                        finish(context, resources, None, done).await;
                        return;
                    }
                    let _ = reply.send(Err(trigger));
                } else {
                    *slot.lock().expect("Git delivery lock poisoned") = Some(result);
                    let delivered = reply.send(Ok(())).is_ok();
                    if delivered {
                        let _ = acknowledgment.await;
                    }
                    let undelivered = slot.lock().expect("Git delivery lock poisoned").take();
                    if let Some(undelivered) = undelivered {
                        super::owner::retain_result(undelivered, &mut resources.destinations);
                        let destinations = std::mem::take(&mut resources.destinations);
                        let (mut partial, removal) = remove_destinations(destinations).await;
                        if removal.is_err() {
                            resources.destinations.append(&mut partial);
                            finish(context, resources, None, done).await;
                            return;
                        }
                    }
                }
            }
        }
    }
    finish(context, resources, None, done).await;
}

/// Removes this worktree and, only after that succeeds, its destination copies
/// and private root. Never touches the repository, primary checkout, or peers.
async fn remove_all(
    context: &Context,
    mut resources: Resources,
) -> (Resources, Result<(), GitError>) {
    if !resources.git_removed {
        match confirm_git_removal(context, &resources.tree).await {
            Ok(()) => resources.git_removed = true,
            Err(error) => return (resources, Err(error)),
        }
    }
    let destinations = std::mem::take(&mut resources.destinations);
    let root = resources.root.take();
    let (destinations, root, result) = tokio::task::spawn_blocking(move || {
        let mut destinations = destinations;
        let result: io::Result<()> = (|| {
            for workspace in &mut destinations {
                workspace.cleanup().map_err(io::Error::other)?;
            }
            if let Some(root) = &root {
                match std::fs::remove_dir_all(root) {
                    Ok(()) => {},
                    Err(error) if error.kind() == io::ErrorKind::NotFound && matches!(std::fs::symlink_metadata(root), Err(missing) if missing.kind() == io::ErrorKind::NotFound) => {},
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        })();
        (destinations, root, result)
    }).await.expect("Git worktree removal worker panicked");
    match result {
        Ok(()) => {
            resources.root = None;
            (resources, Ok(()))
        }
        Err(error) => {
            resources.destinations = destinations;
            resources.root = root;
            (resources, Err(GitError::Cleanup(error)))
        }
    }
}

/// `--force` removes a dirty or untracked-only worktree belonging to this value.
/// `prune` then clears any administrative entry whose directory is gone. When
/// the checkout's repository is already removed, its administrative metadata is
/// gone too, so only this worktree's private directory remains to remove.
async fn confirm_git_removal(context: &Context, tree: &Path) -> Result<(), GitError> {
    let repo = context.checkout_root.join("repo");
    let (repo_present, tree_present) = {
        let repo = repo.clone();
        let tree = tree.to_path_buf();
        tokio::task::spawn_blocking(move || {
            (repo.join(".git").is_dir(), tree.symlink_metadata().is_ok())
        })
        .await
        .expect("Git worktree metadata worker panicked")
    };
    if !repo_present {
        return Ok(());
    }
    let cancel = CancellationToken::new();
    if tree_present {
        let args = vec![
            OsString::from("worktree"),
            OsString::from("remove"),
            OsString::from("--force"),
            tree.as_os_str().to_owned(),
        ];
        super::owner::run(
            &context.checkout_root,
            args,
            None,
            &context.options,
            context.options.timeout,
            context.options.max_output_bytes,
            &cancel,
        )
        .await?;
    }
    let args = vec![
        OsString::from("worktree"),
        OsString::from("prune"),
        OsString::from("--expire"),
        OsString::from("now"),
    ];
    super::owner::run(
        &context.checkout_root,
        args,
        None,
        &context.options,
        context.options.timeout,
        context.options.max_output_bytes,
        &cancel,
    )
    .await?;
    Ok(())
}

async fn remove_destinations(mut destinations: Vec<Workspace>) -> (Vec<Workspace>, io::Result<()>) {
    tokio::task::spawn_blocking(move || {
        let result: io::Result<()> = (|| {
            for workspace in &mut destinations {
                workspace.cleanup().map_err(io::Error::other)?;
            }
            destinations.clear();
            Ok(())
        })();
        (destinations, result)
    })
    .await
    .expect("Git worktree destination worker panicked")
}

async fn finish(
    context: Context,
    mut resources: Resources,
    process: Option<ProcessCleanup>,
    done: watch::Sender<bool>,
) {
    if let Some(process) = process {
        process.wait().await;
    }
    loop {
        let (next, result) = remove_all(&context, resources).await;
        resources = next;
        match result {
            Ok(()) => {
                done.send_replace(true);
                return;
            }
            Err(error) => {
                if let Some(process) = super::owner::pending_process(&error) {
                    process.wait().await;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}
