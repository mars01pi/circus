//! In-memory capture of a [`Workspace`] into a new workspace, and a bounded
//! durable file format for a caller-supplied host path.
//!
//! A [`CodeSnapshot`] can be written with [`CodeSnapshot::save_file`] and read
//! back with [`CodeSnapshot::load_file`]. The archive stores regular-file bytes
//! and directories only. It is not a process or memory checkpoint, not a Docker
//! commit, not a workspace quota, and not encryption, retention, or tenant
//! scoping. The caller owns the host path, its storage, and its lifecycle.

use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use cap_fs_ext::OpenOptionsSyncExt;
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::fs::{Dir, File, OpenOptions};

use super::{Workspace, WorkspaceError, check_path, lock_poisoned};

mod archive;

/// Why a code-snapshot capture, restore, save, or load failed.
///
/// This is not a workspace quota error. [`SnapshotBudget`] is a bound on one
/// capture. [`WorkspaceError`] stays the file-operation error.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// Capture metadata contradicted the workspace: an unsupported node,
    /// malformed manifest, non-portable name, or a payload whose bytes did not
    /// match its declared size.
    #[error("invalid workspace capture manifest: {reason}")]
    InvalidArchive {
        /// Why the capture was rejected, without exposing backend SDK types.
        reason: String,
    },

    /// A transport, manifest, restore, or durable-file limit was zero or
    /// otherwise outside the supported range.
    #[error("invalid archive limit")]
    InvalidArchiveLimit,

    /// A host filesystem operation on a caller-supplied snapshot archive failed.
    ///
    /// `operation` names the failed step, such as `open`, `read`, `create
    /// temporary file`, `write`, `sync`, or `rename`. It never contains the
    /// host path, and neither does `source`.
    #[error("snapshot {operation} failed: {source}")]
    Io {
        /// Failed step.
        operation: &'static str,
        /// Underlying host error.
        #[source]
        source: io::Error,
    },

    /// Snapshot operation was interrupted before staging commit.
    #[error("snapshot operation interrupted before staging commit")]
    Interrupted,
    /// `max_total_bytes` or `max_entries` was zero.
    #[error(
        "snapshot budget must be positive, got max_total_bytes={max_total_bytes} max_entries={max_entries}"
    )]
    InvalidBudget {
        /// Rejected total-byte bound.
        max_total_bytes: usize,
        /// Rejected entry bound.
        max_entries: usize,
    },

    /// The walk saw more files and directories than the capture allows.
    ///
    /// `actual` includes the entry that did not fit. The root directory is not
    /// an entry. No snapshot is returned.
    #[error("snapshot entry count {actual} exceeds capture budget {limit}")]
    EntryLimit {
        /// Configured entry cap.
        limit: usize,
        /// Entries observed before the walk stopped.
        actual: usize,
    },

    /// File bytes, not path lengths, exceeded the capture byte budget.
    ///
    /// `actual` is the detected total: a bounded read's probe byte for direct
    /// workspace capture, or the running sum of manifest-declared sizes before
    /// any payload is read. A file fails first with [`WorkspaceError::LimitExceeded`]
    /// when its per-file cap is tighter.
    #[error("snapshot byte count {actual} exceeds capture budget {limit}")]
    ByteLimit {
        /// Configured total-byte cap.
        limit: usize,
        /// Bytes detected before the walk stopped.
        actual: usize,
    },

    /// A symlink was found. It is not followed and not stored.
    #[error("refusing symlink: {}", path.display())]
    Symlink {
        /// Workspace-relative path. Not a host path.
        path: PathBuf,
    },

    /// A FIFO, device, socket, or other non-regular node was found.
    ///
    /// The node is not opened for reading. Capture fails closed.
    #[error("refusing non-regular node: {}", path.display())]
    NotRegular {
        /// Workspace-relative path. Not a host path.
        path: PathBuf,
    },

    /// A name is not a sequence of normal, portable relative components.
    ///
    /// Each component is checked with the same rules as workspace paths.
    /// `.` and `..` are not stored. This is not a host path.
    #[error("refusing non-portable snapshot path: {}", path.display())]
    NotPortable {
        /// Workspace-relative path that failed the component check.
        path: PathBuf,
    },

    /// A workspace file operation failed, including a closed workspace, a
    /// rejected path, a per-file limit, or I/O.
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),

    /// Restore failed, and deleting the new workspace also failed.
    ///
    /// Drop is not this result. The destination is returned in `destination`
    /// so callers can release an external lock and retry [`Workspace::cleanup`].
    #[error("restore failed ({restore}); destination cleanup failed: {cleanup}")]
    RestoreCleanup {
        /// Why restore stopped.
        restore: Box<SnapshotError>,
        /// The destination workspace whose cleanup can be retried.
        destination: Box<Workspace>,
        /// Why [`Workspace::cleanup`] failed. Not a successful drop.
        #[source]
        cleanup: WorkspaceError,
    },
}

impl From<io::Error> for SnapshotError {
    fn from(err: io::Error) -> Self {
        Self::Workspace(err.into())
    }
}

/// Caller-supplied bound for one [`Workspace::capture_code_snapshot`],
/// [`super::CodeSource::copy_to_workspace`], or
/// [`super::CodeSnapshot::load_file`] operation.
///
/// `max_total_bytes` counts file contents only. `max_entries` counts files and
/// directories, not the root. Both must be positive. This operation budget
/// bounds one capture, copy, or archive load. It is not a workspace total, file-count quota,
/// or host disk quota, and it is not enforced on later writes. There is still
/// no workspace total-quota API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotBudget {
    max_total_bytes: usize,
    max_entries: usize,
}

impl SnapshotBudget {
    /// Rejects a zero byte budget or a zero entry budget.
    pub fn new(max_total_bytes: usize, max_entries: usize) -> Result<Self, SnapshotError> {
        if max_total_bytes == 0 || max_entries == 0 {
            return Err(SnapshotError::InvalidBudget {
                max_total_bytes,
                max_entries,
            });
        }
        Ok(Self {
            max_total_bytes,
            max_entries,
        })
    }

    /// Total file-content bytes this capture or copy may retain.
    pub fn max_total_bytes(&self) -> usize {
        self.max_total_bytes
    }

    /// Files plus directories this operation may retain. The root is not counted.
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }
}

/// Owned, immutable copy of regular files and directories.
///
/// Bytes normally live in this process only; [`Self::save_file`] optionally
/// publishes them to a caller-supplied host path and [`Self::load_file`] reads
/// that bounded format back. Drop frees memory; it does not delete a workspace
/// or an archive. The value is not a process or memory checkpoint, not a
/// quota, and carries no encryption, retention, or tenant scoping. The Docker
/// backend builds this value from a guest workspace enumeration and per-file
/// reads.
///
/// [`Self::restore`] creates a new [`Workspace`]. It does not overwrite the
/// workspace that was captured.
pub struct CodeSnapshot {
    max_file_bytes: usize,
    total_bytes: usize,
    entries: Vec<Entry>,
}

impl std::fmt::Debug for CodeSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeSnapshot")
            .field("max_file_bytes", &self.max_file_bytes)
            .field("total_bytes", &self.total_bytes)
            .field("entry_count", &self.entries.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    body: Body,
}

#[derive(Clone)]
enum Body {
    Dir,
    File(Vec<u8>),
}

/// One backend-neutral node supplied to [`CodeSnapshot::from_entries`].
///
/// Paths are workspace-relative and were already validated by the caller.
pub(crate) struct SnapshotEntry {
    pub path: PathBuf,
    pub body: SnapshotBody,
}

/// Payload of one [`SnapshotEntry`]. Directories carry no payload.
pub(crate) enum SnapshotBody {
    Directory,
    File(Vec<u8>),
}

impl CodeSnapshot {
    /// Builds a snapshot from an already-enumerated, already-budget-checked
    /// workspace tree.
    ///
    /// Entries are stored in the supplied order, which is the sort order of the
    /// capture manifest. `max_file_bytes` is copied into the snapshot and every
    /// payload is re-checked against it, so a caller cannot install a snapshot
    /// that the captured workspace's own file bound would reject. Total bytes
    /// and entry count are derived from the supplied entries; the root is not
    /// an entry.
    pub(crate) fn from_entries(
        max_file_bytes: usize,
        entries: Vec<SnapshotEntry>,
    ) -> Result<Self, SnapshotError> {
        let mut total_bytes = 0usize;
        let mut stored = Vec::new();
        stored.try_reserve(entries.len()).map_err(reserve_error)?;
        for SnapshotEntry { path, body } in entries {
            let body = match body {
                SnapshotBody::Directory => Body::Dir,
                SnapshotBody::File(bytes) => {
                    if bytes.len() > max_file_bytes {
                        return Err(SnapshotError::Workspace(WorkspaceError::LimitExceeded {
                            limit: max_file_bytes,
                            actual: bytes.len(),
                        }));
                    }
                    let Some(total) = total_bytes.checked_add(bytes.len()) else {
                        return Err(SnapshotError::InvalidArchive {
                            reason: "snapshot byte total overflowed".to_string(),
                        });
                    };
                    total_bytes = total;
                    Body::File(bytes)
                }
            };
            stored.push(Entry { path, body });
        }
        Ok(Self {
            max_file_bytes,
            total_bytes,
            entries: stored,
        })
    }

    /// Per-file cap copied from the captured workspace.
    ///
    /// [`Self::restore`] uses this cap; Docker restoration uses target limits.
    pub fn max_file_bytes(&self) -> usize {
        self.max_file_bytes
    }

    /// Files and directories stored, not including the workspace root.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Sum of stored file contents. Directories contribute nothing.
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Creates a new workspace and writes this snapshot into it.
    ///
    /// The source workspace is not required and is not modified. On failure the
    /// new directory is removed with [`Workspace::cleanup`]. If that cleanup
    /// fails, the error is [`SnapshotError::RestoreCleanup`], which owns the
    /// destination so callers can retry cleanup; dropping it is not reported as
    /// success.
    ///
    /// Blocks the calling thread.
    pub fn restore(&self) -> Result<Workspace, SnapshotError> {
        self.restore_with_file_cap(self.max_file_bytes)
    }
    pub(crate) fn restore_with_limits(
        &self,
        max_file_bytes: usize,
        max_total_bytes: usize,
    ) -> Result<Workspace, SnapshotError> {
        if max_file_bytes == 0 || max_file_bytes == usize::MAX {
            return Err(SnapshotError::InvalidArchiveLimit);
        }
        if max_total_bytes == 0 {
            return Err(SnapshotError::InvalidArchiveLimit);
        }
        let mut total = 0usize;
        for entry in &self.entries {
            if let Body::File(bytes) = &entry.body {
                if bytes.len() > max_file_bytes {
                    return Err(SnapshotError::Workspace(WorkspaceError::LimitExceeded {
                        limit: max_file_bytes,
                        actual: bytes.len(),
                    }));
                }
                total = total
                    .checked_add(bytes.len())
                    .ok_or_else(|| SnapshotError::ByteLimit {
                        limit: max_total_bytes,
                        actual: usize::MAX,
                    })?;
                if total > max_total_bytes {
                    return Err(SnapshotError::ByteLimit {
                        limit: max_total_bytes,
                        actual: total,
                    });
                }
            }
        }
        self.restore_with_file_cap(max_file_bytes)
    }

    pub(crate) fn restore_with_file_cap(
        &self,
        max_file_bytes: usize,
    ) -> Result<Workspace, SnapshotError> {
        let workspace = Workspace::new(max_file_bytes)?;
        if let Err(err) = self.populate(&workspace) {
            return Err(abandon_failed_restore(workspace, err));
        }
        Ok(workspace)
    }

    pub(crate) fn populate(&self, workspace: &Workspace) -> Result<(), SnapshotError> {
        for entry in &self.entries {
            if matches!(&entry.body, &Body::Dir) {
                require_portable(&entry.path)?;
                workspace.create_dir_all(&entry.path)?;
            }
        }
        for entry in &self.entries {
            if let Body::File(bytes) = &entry.body {
                require_portable(&entry.path)?;
                workspace.write_file(&entry.path, bytes)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn abandon_failed_restore(
    mut workspace: Workspace,
    restore: SnapshotError,
) -> SnapshotError {
    match workspace.cleanup() {
        Ok(()) => restore,
        Err(cleanup) => SnapshotError::RestoreCleanup {
            restore: Box::new(restore),
            destination: Box::new(workspace),
            cleanup,
        },
    }
}

impl Workspace {
    /// Copies regular files and directories, including empty directories, into
    /// an owned in-memory snapshot.
    ///
    /// The exclusive file lock is taken after the closed check and held for the
    /// whole walk, so this does not interleave with writes on this value. The
    /// walk uses the cap-std directory handle. It does not follow symlinks and
    /// does not open non-regular nodes. A symlink, FIFO, device, socket,
    /// unknown node, or non-portable name fails the capture; nothing is
    /// returned.
    ///
    /// Each relative path, and each of its components, uses the same rejection
    /// rules as [`Self::write_file`]. `max_file_bytes` still caps each file.
    /// `budget` caps this call's retained bytes and entry count only.
    ///
    /// Does not modify this workspace. Blocks the calling thread. Docker's
    /// separate async snapshot API enumerates a running workspace through the
    /// guest and reads each file individually.
    pub fn capture_code_snapshot(
        &self,
        budget: SnapshotBudget,
    ) -> Result<CodeSnapshot, SnapshotError> {
        self.ensure_open()?;
        let _guard = self.file_lock.write().map_err(lock_poisoned)?;
        let dir = self.dir.as_ref().ok_or_else(super::missing_handle)?;
        capture(dir, self.max_file_bytes, budget)
    }
}

struct Frame {
    dir: Dir,
    prefix: PathBuf,
}

struct State {
    entries: Vec<Entry>,
    total_bytes: usize,
}

fn capture(
    root: &Dir,
    max_file_bytes: usize,
    budget: SnapshotBudget,
) -> Result<CodeSnapshot, SnapshotError> {
    let mut state = State {
        entries: Vec::new(),
        total_bytes: 0,
    };
    let mut stack = Vec::new();
    stack.push(Frame {
        dir: root.open_dir(".")?,
        prefix: PathBuf::new(),
    });
    while let Some(frame) = stack.pop() {
        capture_frame(frame, max_file_bytes, budget, &mut state, &mut stack)?;
    }
    Ok(CodeSnapshot {
        max_file_bytes,
        total_bytes: state.total_bytes,
        entries: state.entries,
    })
}

fn capture_frame(
    frame: Frame,
    max_file_bytes: usize,
    budget: SnapshotBudget,
    state: &mut State,
    stack: &mut Vec<Frame>,
) -> Result<(), SnapshotError> {
    for entry in frame.dir.read_dir(".")? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let rel = child_path(&frame.prefix, &name)?;
        let name = Path::new(&name);
        match node_kind(&frame.dir, name, &rel)? {
            Kind::Dir => {
                charge_entry(state, budget)?;
                let child = open_child_dir(&frame.dir, name, &rel)?;
                push_entry(
                    state,
                    Entry {
                        path: rel.clone(),
                        body: Body::Dir,
                    },
                )?;
                stack.try_reserve(1).map_err(reserve_error)?;
                stack.push(Frame {
                    dir: child,
                    prefix: rel,
                });
            }
            Kind::File => {
                charge_entry(state, budget)?;
                let remaining = budget.max_total_bytes.saturating_sub(state.total_bytes);
                let file_limit = max_file_bytes.min(remaining);
                let bytes = match read_capped(&frame.dir, name, &rel, file_limit) {
                    Ok(bytes) => bytes,
                    Err(SnapshotError::Workspace(WorkspaceError::LimitExceeded {
                        actual, ..
                    })) if file_limit < max_file_bytes => {
                        let actual = state.total_bytes.saturating_add(actual);
                        return Err(SnapshotError::ByteLimit {
                            limit: budget.max_total_bytes,
                            actual,
                        });
                    }
                    Err(err) => return Err(err),
                };
                charge_bytes(state, budget, bytes.len())?;
                push_entry(
                    state,
                    Entry {
                        path: rel,
                        body: Body::File(bytes),
                    },
                )?;
            }
        }
    }
    Ok(())
}

pub(super) enum Kind {
    Dir,
    File,
}

pub(super) fn child_path(prefix: &Path, name: &std::ffi::OsStr) -> Result<PathBuf, SnapshotError> {
    let name_path = Path::new(name);
    require_portable_component(name_path)?;
    let rel = append_component(prefix, name);
    require_portable(&rel)?;
    Ok(rel)
}

fn append_component(prefix: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let mut raw = std::ffi::OsString::new();
    if !prefix.as_os_str().is_empty() {
        raw.push(prefix.as_os_str());
        raw.push("/");
    }
    raw.push(name);
    PathBuf::from(raw)
}

fn require_portable(path: &Path) -> Result<(), SnapshotError> {
    check_path(path)?;
    let mut normal = 0usize;
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                check_path(Path::new(name))?;
                normal += 1;
            }
            _ => {
                return Err(SnapshotError::NotPortable {
                    path: path.to_path_buf(),
                });
            }
        }
    }
    if normal == 0 {
        return Err(SnapshotError::NotPortable {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn require_portable_component(path: &Path) -> Result<(), SnapshotError> {
    check_path(path)?;
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(name)) if components.next().is_none() => {
            check_path(Path::new(name))?;
            Ok(())
        }
        _ => Err(SnapshotError::NotPortable {
            path: path.to_path_buf(),
        }),
    }
}

pub(super) fn node_kind(dir: &Dir, name: &Path, rel: &Path) -> Result<Kind, SnapshotError> {
    let meta = dir.symlink_metadata(name)?;
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        return Err(SnapshotError::Symlink {
            path: rel.to_path_buf(),
        });
    }
    if file_type.is_dir() {
        return Ok(Kind::Dir);
    }
    if file_type.is_file() {
        return Ok(Kind::File);
    }
    Err(SnapshotError::NotRegular {
        path: rel.to_path_buf(),
    })
}

pub(super) fn open_child_dir(parent: &Dir, name: &Path, rel: &Path) -> Result<Dir, SnapshotError> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.follow(FollowSymlinks::No);
    options.maybe_dir(true);
    #[cfg(unix)]
    options.nonblock(true);
    let file = match parent.open_with(name, &options) {
        Ok(file) => file,
        Err(err) => return Err(classify_failed_open(parent, name, rel, err)),
    };
    refuse_unwanted_handle(&file, rel, true)?;
    Ok(Dir::from_std_file(file.into_std()))
}

pub(super) fn read_capped(
    parent: &Dir,
    name: &Path,
    rel: &Path,
    limit: usize,
) -> Result<Vec<u8>, SnapshotError> {
    let mut bytes = Vec::new();
    read_capped_into(parent, name, rel, limit, &mut bytes)?;
    Ok(bytes)
}

pub(super) fn read_capped_into(
    parent: &Dir,
    name: &Path,
    rel: &Path,
    limit: usize,
    bytes: &mut Vec<u8>,
) -> Result<(), SnapshotError> {
    let file = open_regular(parent, name, rel)?;
    match super::read_regular_into(file, rel, limit, bytes) {
        Ok(()) => Ok(()),
        Err(WorkspaceError::NotRegularFile { path }) => Err(SnapshotError::NotRegular { path }),
        Err(err) => Err(err.into()),
    }
}

pub(super) fn open_regular(parent: &Dir, name: &Path, rel: &Path) -> Result<File, SnapshotError> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        options.nonblock(true);
    }
    let file = match parent.open_with(name, &options) {
        Ok(file) => file,
        Err(err) => return Err(classify_failed_open(parent, name, rel, err)),
    };
    refuse_unwanted_handle(&file, rel, false)?;
    Ok(file)
}

fn refuse_unwanted_handle(file: &File, rel: &Path, want_dir: bool) -> Result<(), SnapshotError> {
    let meta = file.metadata()?;
    if meta.is_symlink() {
        return Err(SnapshotError::Symlink {
            path: rel.to_path_buf(),
        });
    }
    let ok = if want_dir {
        meta.is_dir()
    } else {
        meta.is_file()
    };
    if ok {
        return Ok(());
    }
    Err(SnapshotError::NotRegular {
        path: rel.to_path_buf(),
    })
}

fn classify_failed_open(parent: &Dir, name: &Path, rel: &Path, err: io::Error) -> SnapshotError {
    match parent.symlink_metadata(name) {
        Ok(meta) if meta.is_symlink() => SnapshotError::Symlink {
            path: rel.to_path_buf(),
        },
        Ok(meta) if !meta.is_file() && !meta.is_dir() => SnapshotError::NotRegular {
            path: rel.to_path_buf(),
        },
        _ => err.into(),
    }
}

fn charge_entry(state: &State, budget: SnapshotBudget) -> Result<(), SnapshotError> {
    let Some(next) = state.entries.len().checked_add(1) else {
        return Err(oom("snapshot entry count overflowed"));
    };
    if next > budget.max_entries {
        return Err(SnapshotError::EntryLimit {
            limit: budget.max_entries,
            actual: next,
        });
    }
    Ok(())
}

fn charge_bytes(
    state: &mut State,
    budget: SnapshotBudget,
    add: usize,
) -> Result<(), SnapshotError> {
    let Some(next) = state.total_bytes.checked_add(add) else {
        return Err(oom("snapshot byte total overflowed"));
    };
    if next > budget.max_total_bytes {
        return Err(SnapshotError::ByteLimit {
            limit: budget.max_total_bytes,
            actual: next,
        });
    }
    state.total_bytes = next;
    Ok(())
}

fn push_entry(state: &mut State, entry: Entry) -> Result<(), SnapshotError> {
    state.entries.try_reserve(1).map_err(reserve_error)?;
    state.entries.push(entry);
    Ok(())
}

fn reserve_error(err: std::collections::TryReserveError) -> SnapshotError {
    io::Error::new(ErrorKind::OutOfMemory, err).into()
}

fn oom(message: &str) -> SnapshotError {
    io::Error::new(ErrorKind::OutOfMemory, message).into()
}
