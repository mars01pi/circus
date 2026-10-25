//! Explicitly authorized source directories copied into owned workspaces.

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use super::snapshot::{self, Kind};
use super::{SnapshotBudget, SnapshotError, Workspace, WorkspaceError};

/// Failure to open an authorized source or copy its contents.
///
/// A failed copy is never published as a successful workspace. If explicit
/// cleanup also fails, [`Self::CopyCleanup`] retains the partial destination.
#[derive(Debug, thiserror::Error)]
pub enum CodeCopyError {
    /// Source I/O failed. During opening, `path` is the caller's source path;
    /// during walking it is source-relative (the root is `.`).
    #[error("source I/O failed at {}: {source}", path.display())]
    SourceIo {
        /// Source path associated with the failed operation.
        path: PathBuf,
        /// Original filesystem error.
        #[source]
        source: io::Error,
    },

    /// A symlink was encountered; source descendants are never followed.
    #[error("refusing source symlink: {}", path.display())]
    Symlink {
        /// Source-relative path, not a host root path.
        path: PathBuf,
    },

    /// A FIFO, device, socket, or other non-regular node was encountered.
    #[error("refusing non-regular source node: {}", path.display())]
    NotRegular {
        /// Source-relative path, not a host root path.
        path: PathBuf,
    },

    /// A name was not UTF-8 or failed the workspace-relative component rules.
    #[error("refusing non-portable source path: {}", path.display())]
    NotPortable {
        /// Source-relative path that could not be copied.
        path: PathBuf,
    },

    /// Files plus directories exceeded the operation's entry budget.
    ///
    /// The root is excluded; empty directories and zero-byte files count.
    #[error("copy entry count {actual} exceeds budget {limit}")]
    EntryLimit {
        /// Configured maximum entries.
        limit: usize,
        /// Count including the first entry that did not fit.
        actual: usize,
    },

    /// File-content bytes exceeded the operation's total-byte budget.
    #[error("copy byte count {actual} exceeds budget {limit}")]
    ByteLimit {
        /// Configured maximum total file-content bytes.
        limit: usize,
        /// Bytes detected including the first probe byte beyond the limit,
        /// not an untrusted metadata length.
        actual: usize,
    },

    /// Destination creation/writing failed, or a source file exceeded the
    /// per-file limit. The original workspace error and its source are retained.
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),

    /// Copy failed and explicit destination cleanup also failed.
    ///
    /// Match this variant to recover `destination`, release external locks,
    /// and retry [`Workspace::cleanup`]. Dropping it only attempts best-effort
    /// cleanup; drop is not reported as successful cleanup.
    #[error("copy failed ({copy}); destination cleanup failed: {cleanup}")]
    CopyCleanup {
        /// Original copy failure, retaining its error source.
        copy: Box<CodeCopyError>,
        /// Owned partial destination, available for explicit cleanup retry.
        destination: Box<Workspace>,
        /// Original cleanup failure.
        #[source]
        cleanup: WorkspaceError,
    },
}

/// Read-only directory capability explicitly authorized by a trusted caller.
///
/// This type grants no authority to an untrusted tenant merely because it
/// supplied a path. The library caller must authorize the source before
/// [`Self::open`]. Opening follows host path semantics, including links in the
/// root path; descendant access thereafter uses only capability-relative,
/// no-follow opens. The root path and directory handle are not exposed.
///
/// Copies are independent owned [`Workspace`] values, not mounts, Git clones,
/// worktrees, or [`super::CodeSnapshot`] values. No Git metadata is specially
/// excluded and no source credentials are implicitly shared. Ordinary source
/// files, including credential files if present, are copied, so callers must
/// authorize the complete selected tree accordingly.
///
/// This API blocks the calling thread. Async callers must use `spawn_blocking`
/// or an equivalent; aborting a started blocking task cannot stop its copy.
/// The capability is read-only through this API, not an OS-level exclusion of
/// external writers. Concurrent external changes may cause errors or a mixed
/// view: this is not a consistent snapshot or a crash-atomic operation.
pub struct CodeSource {
    dir: Dir,
}

impl std::fmt::Debug for CodeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeSource").finish_non_exhaustive()
    }
}

impl CodeSource {
    /// Opens a source directory explicitly authorized for reading by the caller.
    ///
    /// Uses ambient authority only for this root open. The path may be absolute
    /// or relative and resolves according to host filesystem semantics; it is
    /// not itself a tenant authorization policy. No source writes are offered.
    /// A missing/inaccessible source or non-directory root is
    /// [`CodeCopyError::SourceIo`]. Blocks the calling thread.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CodeCopyError> {
        let path = path.as_ref();
        let dir = Dir::open_ambient_dir(path, ambient_authority())
            .map_err(|source| source_io(path, source))?;
        Ok(Self { dir })
    }

    /// Copies regular files and all directories into a fresh owned workspace.
    ///
    /// Source contents are read only; each call creates an independent
    /// destination. Symlinks (even internal ones), special nodes, and names
    /// that are not UTF-8 portable workspace-relative components fail closed.
    /// No-follow, nonblocking source opens and opened-handle type checks reject
    /// nodes replaced between enumeration and open without waiting for FIFO
    /// peers on Unix. Descendant access remains capability-confined.
    ///
    /// `max_file_bytes` has exactly [`Workspace::new`]'s per-file semantics.
    /// `budget` counts file-content bytes and files plus directories, excluding
    /// the root. Exact bounds are accepted; empty files consume an entry and
    /// empty directories are preserved. These are this copy's bounds only,
    /// not a quota on later workspace writes. Files are read one at a time
    /// into a reusable buffer using a bounded read with a one-byte over-limit
    /// probe, never sizing an allocation from source metadata or retaining
    /// the whole tree's payload.
    ///
    /// On failure explicit cleanup runs before returning; if it fails,
    /// [`CodeCopyError::CopyCleanup`] owns the partial destination for retry.
    /// This is blocking, not crash-atomic, and not a consistent snapshot if
    /// another process changes the source. Async callers must use
    /// `spawn_blocking`; a started blocking task cannot be stopped by abort.
    pub fn copy_to_workspace(
        &self,
        max_file_bytes: usize,
        budget: SnapshotBudget,
    ) -> Result<Workspace, CodeCopyError> {
        let workspace = Workspace::new(max_file_bytes)?;
        if let Err(copy) = self.populate(&workspace, max_file_bytes, budget, false) {
            return Err(abandon_failed_copy(workspace, copy));
        }
        Ok(workspace)
    }

    pub(crate) fn copy_to_workspace_excluding_git(
        &self,
        max_file_bytes: usize,
        budget: SnapshotBudget,
    ) -> Result<Workspace, CodeCopyError> {
        let workspace = Workspace::new(max_file_bytes)?;
        if let Err(copy) = self.populate(&workspace, max_file_bytes, budget, true) {
            return Err(abandon_failed_copy(workspace, copy));
        }
        Ok(workspace)
    }

    fn populate(
        &self,
        destination: &Workspace,
        max_file_bytes: usize,
        budget: SnapshotBudget,
        exclude_git: bool,
    ) -> Result<(), CodeCopyError> {
        let root = self
            .dir
            .open_dir(".")
            .map_err(|err| source_io(Path::new("."), err))?;
        let mut stack = Vec::new();
        push_frame(
            &mut stack,
            Frame {
                dir: root,
                prefix: PathBuf::new(),
            },
        )?;
        let mut entries = 0usize;
        let mut total_bytes = 0usize;
        let mut bytes = Vec::new();
        while let Some(frame) = stack.pop() {
            let relative_dir = if frame.prefix.as_os_str().is_empty() {
                Path::new(".")
            } else {
                &frame.prefix
            };
            let listing = frame
                .dir
                .entries()
                .map_err(|err| source_io(relative_dir, err))?;
            for entry in listing {
                let entry = entry.map_err(|err| source_io(relative_dir, err))?;
                let name = entry.file_name();
                if exclude_git && frame.prefix.as_os_str().is_empty() && name == OsStr::new(".git")
                {
                    continue;
                }
                let rel = snapshot::child_path(&frame.prefix, &name)
                    .map_err(|err| source_error(err, &frame.prefix.join(&name)))?;
                if name.to_str().is_none() {
                    return Err(CodeCopyError::NotPortable { path: rel });
                }
                let kind = snapshot::node_kind(&frame.dir, Path::new(&name), &rel)
                    .map_err(|err| source_error(err, &rel))?;
                let next = entries.checked_add(1).ok_or_else(|| {
                    source_io(&rel, io::Error::other("copy entry count overflowed"))
                })?;
                if next > budget.max_entries() {
                    return Err(CodeCopyError::EntryLimit {
                        limit: budget.max_entries(),
                        actual: next,
                    });
                }
                entries = next;
                match kind {
                    Kind::Dir => {
                        let child = snapshot::open_child_dir(&frame.dir, Path::new(&name), &rel)
                            .map_err(|err| source_error(err, &rel))?;
                        destination.create_dir_all(&rel)?;
                        push_frame(
                            &mut stack,
                            Frame {
                                dir: child,
                                prefix: rel,
                            },
                        )?;
                    }
                    Kind::File => {
                        let limit = max_file_bytes.min(budget.max_total_bytes() - total_bytes);
                        match snapshot::read_capped_into(
                            &frame.dir,
                            Path::new(&name),
                            &rel,
                            limit,
                            &mut bytes,
                        ) {
                            Ok(()) => (),
                            Err(SnapshotError::Workspace(WorkspaceError::LimitExceeded {
                                actual,
                                ..
                            })) if limit < max_file_bytes => {
                                return Err(CodeCopyError::ByteLimit {
                                    limit: budget.max_total_bytes(),
                                    actual: total_bytes.saturating_add(actual),
                                });
                            }
                            Err(err) => return Err(source_error(err, &rel)),
                        }
                        total_bytes += bytes.len();
                        destination.write_file(&rel, &bytes)?;
                    }
                }
            }
        }
        Ok(())
    }
}

struct Frame {
    dir: Dir,
    prefix: PathBuf,
}

fn push_frame(stack: &mut Vec<Frame>, frame: Frame) -> Result<(), CodeCopyError> {
    stack.try_reserve(1).map_err(|err| {
        source_io(
            &frame.prefix,
            io::Error::new(io::ErrorKind::OutOfMemory, err),
        )
    })?;
    stack.push(frame);
    Ok(())
}

fn source_io(path: &Path, source: io::Error) -> CodeCopyError {
    CodeCopyError::SourceIo {
        path: path.to_path_buf(),
        source,
    }
}

fn source_error(error: SnapshotError, rel: &Path) -> CodeCopyError {
    match error {
        SnapshotError::Symlink { path } => CodeCopyError::Symlink { path },
        SnapshotError::NotRegular { path } => CodeCopyError::NotRegular { path },
        SnapshotError::NotPortable { .. }
        | SnapshotError::Workspace(WorkspaceError::PathRejected { .. }) => {
            CodeCopyError::NotPortable {
                path: rel.to_path_buf(),
            }
        }
        SnapshotError::Workspace(WorkspaceError::Io(source)) => source_io(rel, source),
        SnapshotError::Workspace(error) => CodeCopyError::Workspace(error),
        // Only path, node, open, and bounded-read helpers are used here.
        // Preserve the full source if those helpers ever add another failure.
        error => source_io(rel, io::Error::other(error)),
    }
}

fn abandon_failed_copy(mut destination: Workspace, copy: CodeCopyError) -> CodeCopyError {
    match destination.cleanup() {
        Ok(()) => copy,
        Err(cleanup) => CodeCopyError::CopyCleanup {
            copy: Box::new(copy),
            destination: Box::new(destination),
            cleanup,
        },
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn failed_copy_retains_destination_for_cleanup_retry() {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        let workspace = Workspace::new(8).unwrap();
        let path = workspace.temp.as_ref().unwrap().path().join("held");
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(path)
            .unwrap();
        let copy = CodeCopyError::ByteLimit {
            limit: 1,
            actual: 2,
        };
        let error = abandon_failed_copy(workspace, copy);
        let CodeCopyError::CopyCleanup {
            copy, destination, ..
        } = error
        else {
            panic!("expected copy cleanup error");
        };
        assert!(matches!(
            *copy,
            CodeCopyError::ByteLimit {
                limit: 1,
                actual: 2
            }
        ));
        drop(held);
        let mut destination = *destination;
        destination.cleanup().unwrap();
        assert!(matches!(
            destination.read_file("held"),
            Err(WorkspaceError::Closed)
        ));
    }
}
