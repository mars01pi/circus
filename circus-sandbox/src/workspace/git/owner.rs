use super::*;
use crate::infrastructure::git_process::{GitCommand, ProcessOutput, run_owned};
use crate::runtime::ProcessCleanup;
use std::{ffi::OsString, io, path::Path};
use tokio::time::Instant;

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
    Worktree {
        started: Instant,
        reply: oneshot::Sender<Result<super::GitWorktree, GitError>>,
    },
    Cleanup(oneshot::Sender<Result<(), GitError>>),
}
struct Resources {
    root: Option<PathBuf>,
    destinations: Vec<Workspace>,
}
impl Resources {
    fn remove(&mut self) -> io::Result<()> {
        for workspace in &mut self.destinations {
            workspace.cleanup().map_err(io::Error::other)?;
        }
        self.destinations.clear();
        if let Some(root) = &self.root {
            match std::fs::remove_dir_all(root) {
                Ok(()) => {}
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        && matches!(std::fs::symlink_metadata(root), Err(missing) if missing.kind() == io::ErrorKind::NotFound) =>
                    {}
                Err(error) => return Err(error),
            }
        }
        self.root = None;
        Ok(())
    }
}
async fn remove(resources: Resources) -> (Resources, io::Result<()>) {
    tokio::task::spawn_blocking(move || {
        let mut resources = resources;
        let result = resources.remove();
        (resources, result)
    })
    .await
    .expect("Git removal worker panicked")
}
pub(super) fn required(source: GitError, cleanup: &GitCleanup) -> GitError {
    GitError::CleanupRequired {
        source: Box::new(source),
        cleanup: cleanup.clone(),
    }
}
pub(super) fn pending_process(error: &GitError) -> Option<ProcessCleanup> {
    match error {
        GitError::Process(ProcessError::Cleanup { cleanup, .. }) => Some(cleanup.clone()),
        _ => None,
    }
}
async fn finish(
    mut resources: Resources,
    process: Option<ProcessCleanup>,
    done: watch::Sender<bool>,
) {
    if let Some(process) = process {
        process.wait().await;
    }
    loop {
        let (next, result) = remove(resources).await;
        resources = next;
        if result.is_ok() {
            done.send_replace(true);
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
fn allocate(storage: PathBuf) -> (Resources, io::Result<()>) {
    let mut resources = Resources {
        root: None,
        destinations: Vec::new(),
    };
    let result = (|| {
        std::fs::create_dir_all(&storage)?;
        let temp = tempfile::Builder::new()
            .prefix("circus-git-")
            .tempdir_in(storage)?;
        let root = temp.keep();
        resources.root = Some(root.clone());
        for name in ["repo", "home", "config", "empty-hooks", "tmp"] {
            std::fs::create_dir(root.join(name))?;
        }
        std::fs::write(root.join("config/global"), b"")?;
        std::fs::write(root.join("config/system"), b"")?;
        Ok(())
    })();
    (resources, result)
}

pub(super) async fn serve(
    source: GitSource,
    revision: GitRevision,
    options: GitOptions,
    shutdown: CancellationToken,
    mut requests: mpsc::UnboundedReceiver<Request>,
    ready: oneshot::Sender<Result<(), GitError>>,
) {
    let deadline = Instant::now() + options.timeout;
    let (done, receive) = watch::channel(false);
    let cleanup = GitCleanup { done: receive };
    let storage = options.storage_root.clone();
    let (mut resources, allocation) = tokio::task::spawn_blocking(move || allocate(storage))
        .await
        .expect("Git allocation worker panicked");
    let preparation = match allocation {
        Ok(()) => {
            prepare(
                resources.root.as_ref().expect("allocated root"),
                source,
                &revision,
                &options,
                deadline,
                &shutdown,
            )
            .await
        }
        Err(error) => Err(GitError::Cleanup(error)),
    };
    if let Err(error) = preparation {
        let process = pending_process(&error);
        if process.is_some() {
            let _ = ready.send(Err(required(error, &cleanup)));
            finish(resources, process, done).await;
        } else {
            let (next, result) = remove(resources).await;
            resources = next;
            if result.is_ok() {
                done.send_replace(true);
                let _ = ready.send(Err(error));
            } else {
                let _ = ready.send(Err(required(error, &cleanup)));
                finish(resources, None, done).await;
            }
        }
        return;
    }
    if ready.send(Ok(())).is_err() {
        finish(resources, None, done).await;
        return;
    }
    loop {
        let request = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            request = requests.recv() => match request { Some(request) => request, None => break },
        };
        match request {
            Request::Cleanup(reply) => {
                let (next, result) = remove(resources).await;
                resources = next;
                match result {
                    Ok(()) => {
                        done.send_replace(true);
                        let _ = reply.send(Ok(()));
                    }
                    Err(error) => {
                        let _ = reply.send(Err(required(GitError::Cleanup(error), &cleanup)));
                        finish(resources, None, done).await;
                        // Preserve idempotent cleanup replies after background retry.
                        while let Some(request) = requests.recv().await {
                            match request {
                                Request::Cleanup(reply) => {
                                    let _ = reply.send(Ok(()));
                                }
                                Request::Copy { reply, .. } => {
                                    let _ = reply.send(Err(GitError::Closed));
                                }
                                Request::Worktree { reply, .. } => {
                                    let _ = reply.send(Err(GitError::Closed));
                                }
                            }
                        }
                        return;
                    }
                }
            }
            Request::Worktree { started, reply } => {
                let Some(root) = resources.root.clone() else {
                    let _ = reply.send(Err(GitError::Closed));
                    continue;
                };
                let cancel = shutdown.child_token();
                let deadline = started + options.timeout;
                let result = super::worktree::create(
                    root,
                    options.clone(),
                    revision.clone(),
                    deadline,
                    &cancel,
                )
                .await;
                // A dropped receiver drops the new worktree handle, whose independent
                // owner then removes the directory and administrative entry.
                let _ = reply.send(result);
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
                let Some(root) = &resources.root else {
                    let _ = reply.send(Err(GitError::Closed));
                    continue;
                };
                let deadline = started + options.timeout;
                if let Err(error) = remaining(deadline, &cancel) {
                    let _ = reply.send(Err(error));
                    continue;
                }
                let repo = root.join("repo");
                let mut task = tokio::task::spawn_blocking(move || {
                    super::super::CodeSource::open(repo)?
                        .copy_to_workspace_excluding_git(max_file_bytes, budget)
                });
                let (result, trigger) = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => (task.await, Some(GitError::Process(ProcessError::Cancelled))),
                    _ = tokio::time::sleep_until(deadline) => (task.await, Some(GitError::Process(ProcessError::Timeout))),
                    result = &mut task => (result, None),
                };
                let result = result
                    .expect("Git copy worker panicked")
                    .map_err(GitError::Copy);
                if let Some(trigger) = trigger {
                    retain_result(result, &mut resources.destinations);
                    // Cancellation ends this checkout only when a destination cannot
                    // be removed. This keeps the failed destination worker-owned.
                    let destinations = std::mem::take(&mut resources.destinations);
                    let (mut partial, removal) = remove(Resources {
                        root: None,
                        destinations,
                    })
                    .await;
                    if removal.is_err() {
                        resources.destinations.append(&mut partial.destinations);
                        let _ = reply.send(Err(required(trigger, &cleanup)));
                        finish(resources, None, done).await;
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
                        retain_result(undelivered, &mut resources.destinations);
                        let destinations = std::mem::take(&mut resources.destinations);
                        let (mut partial, removal) = remove(Resources {
                            root: None,
                            destinations,
                        })
                        .await;
                        if removal.is_err() {
                            resources.destinations.append(&mut partial.destinations);
                            finish(resources, None, done).await;
                            return;
                        }
                    }
                }
            }
        }
    }
    finish(resources, None, done).await;
}
pub(super) fn retain_result(
    result: Result<Workspace, GitError>,
    destinations: &mut Vec<Workspace>,
) {
    match result {
        Ok(workspace) => destinations.push(workspace),
        Err(GitError::Copy(CodeCopyError::CopyCleanup { destination, .. })) => {
            destinations.push(*destination)
        }
        _ => {}
    }
}
pub(super) fn remaining(
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<Duration, GitError> {
    if cancel.is_cancelled() {
        return Err(ProcessError::Cancelled.into());
    }
    deadline
        .checked_duration_since(Instant::now())
        .filter(|time| !time.is_zero())
        .ok_or_else(|| ProcessError::Timeout.into())
}
async fn prepare(
    root: &Path,
    source: GitSource,
    revision: &GitRevision,
    options: &GitOptions,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<(), GitError> {
    let (protocol, source) = match source.0 {
        SourceKind::Local(path) => ("file", path.into_os_string()),
        SourceKind::Https(url) => ("https", OsString::from(url.as_str())),
    };
    let mut output_left = options.max_output_bytes;
    let commands = [
        vec![
            "init".into(),
            "--quiet".into(),
            "--object-format".into(),
            if revision.0.len() == 64 {
                "sha256".into()
            } else {
                "sha1".into()
            },
        ],
        vec![
            "fetch".into(),
            "--depth=1".into(),
            "--no-tags".into(),
            "--no-recurse-submodules".into(),
            "--".into(),
            source,
            revision.0.as_str().into(),
        ],
        vec![
            "checkout".into(),
            "--detach".into(),
            "--quiet".into(),
            revision.0.as_str().into(),
        ],
        vec!["rev-parse".into(), "--verify".into(), "HEAD".into()],
    ];
    let mut head = Vec::new();
    for args in commands {
        let output = run(
            root,
            args,
            Some(protocol),
            options,
            remaining(deadline, cancel)?,
            output_left,
            cancel,
        )
        .await?;
        output_left -= output.stdout.len() + output.stderr.len();
        head = output.stdout;
    }
    remaining(deadline, cancel)?;
    if String::from_utf8_lossy(&head).trim() != revision.0 {
        return Err(GitError::Revision("git returned a different commit".into()));
    }
    Ok(())
}
pub(super) async fn run(
    root: &Path,
    args: Vec<OsString>,
    protocol: Option<&str>,
    options: &GitOptions,
    timeout: Duration,
    output_limit: usize,
    cancel: &CancellationToken,
) -> Result<ProcessOutput, GitError> {
    let mut hardened = Vec::new();
    for config in [
        "protocol.allow=never",
        "core.fsmonitor=false",
        "submodule.recurse=false",
        "fetch.recurseSubmodules=false",
        "credential.helper=",
        "credential.interactive=false",
        "core.autocrlf=false",
        "core.symlinks=true",
        "init.templateDir=",
    ] {
        hardened.extend([OsString::from("-c"), OsString::from(config)]);
    }
    if let Some(protocol) = protocol {
        hardened.extend([
            "-c".into(),
            format!("protocol.{protocol}.allow=always").into(),
        ]);
    }
    hardened.extend(["-c".into()]);
    let mut hooks = OsString::from("core.hooksPath=");
    hooks.push(root.join("empty-hooks"));
    hardened.push(hooks);
    hardened.extend(args);
    let mut environment: Vec<(OsString, OsString)> = [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_ATTR_NOSYSTEM", "1"),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value.into()))
    .collect();
    for (key, relative) in [
        ("HOME", "home"),
        ("USERPROFILE", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("GIT_CONFIG_GLOBAL", "config/global"),
        ("GIT_CONFIG_SYSTEM", "config/system"),
        ("TMPDIR", "tmp"),
        ("TMP", "tmp"),
        ("TEMP", "tmp"),
    ] {
        environment.push((key.into(), root.join(relative).into_os_string()));
    }
    #[cfg(windows)]
    if let Some(system) = std::env::var_os("SystemRoot") {
        environment.push(("SystemRoot".into(), system));
    }
    let out = run_owned(
        GitCommand {
            executable: options.executable.clone(),
            args: hardened,
            cwd: root.join("repo"),
            environment,
            timeout,
            output_limit,
        },
        cancel.clone(),
    )
    .await?;
    if !out.status.success() {
        return Err(GitError::Command { status: out.status });
    }
    Ok(out)
}
