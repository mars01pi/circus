mod code_source;
mod git;
mod snapshot;

use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{PoisonError, RwLock};

#[cfg(unix)]
use cap_fs_ext::OpenOptionsSyncExt;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, OpenOptions};

/// Why [`Workspace`] rejected a path before touching the filesystem.
///
/// Illegal paths are empty, contain `\`, have a Windows drive prefix
/// (`[A-Za-z]:` on any component) or a leading `//`, are absolute (a leading
/// `/` or [`Path::is_absolute`]), or contain a `..` component. `.` and
/// `foo/./bar` are allowed. A colon that is not a one-letter drive prefix,
/// such as `file:name` or `CC:name`, is not this prefix. Separators in
/// accepted paths must be `/`. A successful cleanup is
/// [`WorkspaceError::Closed`], not one of these path categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathRejectReason {
    /// The path was empty.
    Empty,
    /// The path was absolute.
    Absolute,
    /// The path contained a `..` component.
    ParentComponent,
    /// The path had a Windows drive prefix on any component, or started with `//`.
    ///
    /// A leading `//` or drive prefix is reported before [`Self::Absolute`].
    /// A `..` component is still [`Self::ParentComponent`] when that check
    /// matches first. `\` is [`Self::Backslash`].
    WindowsPrefix,
    /// The path contained `\`.
    Backslash,
}

impl std::fmt::Display for PathRejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Empty => "empty",
            Self::Absolute => "absolute",
            Self::ParentComponent => "parent component",
            Self::WindowsPrefix => "Windows prefix",
            Self::Backslash => "backslash",
        })
    }
}

/// Failure of a workspace constructor or filesystem operation.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// [`Workspace::cleanup`] already succeeded. Further file operations fail
    /// with this error, including operations whose paths would otherwise be
    /// rejected. A failed cleanup does **not** produce this error.
    #[error("workspace is closed")]
    Closed,

    /// `max_file_bytes` was `0` or [`usize::MAX`]. The cap must be in
    /// `1..=usize::MAX - 1` so a one-byte over-limit read probe cannot overflow.
    #[error("max_file_bytes must be in 1..=usize::MAX - 1, got {max_file_bytes}")]
    InvalidLimit {
        /// The rejected cap.
        max_file_bytes: usize,
    },

    /// The path is not a workspace-relative path. `path` is the original value,
    /// not a canonicalized or rooted path.
    #[error("path rejected ({reason}): {}", path.display())]
    PathRejected {
        /// The caller-supplied path.
        path: PathBuf,
        /// Which check failed.
        reason: PathRejectReason,
    },

    /// A write was longer than the cap, or a read proved the file is longer
    /// than the cap. For writes, `actual` is `contents.len()`. For reads,
    /// `actual` is the number of bytes observed before stopping (`limit + 1`),
    /// not the unknown full file size.
    #[error("byte length {actual} exceeds workspace file limit {limit}")]
    LimitExceeded {
        /// Configured maximum file size in bytes.
        limit: usize,
        /// Write length, or the read probe length that exceeded `limit`.
        actual: usize,
    },

    /// The path exists but is not a regular file. Directories, devices, FIFOs,
    /// and other non-regular nodes are not read or truncated. An in-workspace
    /// symlink is followed; a non-regular target is rejected from the opened
    /// handle and the open does not wait for a FIFO peer.
    #[error("not a regular file: {}", path.display())]
    NotRegularFile {
        /// The caller-supplied path.
        path: PathBuf,
    },

    /// An underlying filesystem error. Missing files and missing parents are
    /// reported here, typically with [`std::io::ErrorKind::NotFound`].
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A temporary directory opened as a capability handle.
///
/// File operations use that handle and block the calling thread. Async callers
/// must use `spawn_blocking` (or an equivalent) on their own runtime. Drop
/// deletes the directory on a best-effort basis and ignores errors; call
/// [`Workspace::cleanup`] to observe removal failures.
///
/// Calls on the same value share one [`RwLock`]: [`Self::read_file`] may run
/// alongside other reads, while [`Self::write_file`], [`Self::create_dir_all`],
/// [`Self::remove_file`], and [`Self::capture_code_snapshot`] are exclusive
/// with each other and with reads. The guard is acquired only after the caller
/// path has been borrowed and checked, and it is held from the final open
/// through the end of that read, write, or capture. A poisoned lock becomes
/// [`WorkspaceError::Io`]; it does not panic. [`Self::cleanup`] takes `&mut self`
/// and does not lock.
///
/// That lock only orders calls on this value in this process. It is not
/// process isolation, not a workspace or disk quota, and not a crash-safe
/// atomic replace. On Unix the root is created mode `0o700`, which does
/// not hide it from same-account or privileged processes. `max_file_bytes`
/// is a per-file cap. This crate has no total-quota API.
///
/// This type does not sandbox a process and does not claim container support.
pub struct Workspace {
    dir: Option<Dir>,
    temp: Option<tempfile::TempDir>,
    max_file_bytes: usize,
    file_lock: RwLock<()>,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("max_file_bytes", &self.max_file_bytes)
            .field("closed", &self.temp.is_none())
            .finish_non_exhaustive()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        // Close the capability before the temp directory so Windows can delete it.
        // Errors are intentionally discarded; `cleanup` is the observable path.
        self.dir.take();
        self.temp.take();
    }
}

impl Workspace {
    /// Creates a temporary directory and opens it with ambient authority.
    ///
    /// On Unix the directory is created with mode `0o700`. That mode is an
    /// argument to `mkdir` via [`tempfile::Builder::permissions`]; it is not
    /// applied by a later `chmod`, and this function does not change the
    /// process umask. The kernel umask may clear more bits, but it cannot add
    /// group or other permission, so other host users cannot traverse the
    /// directory. Same-account processes and privileged processes are not
    /// isolated. Windows does not set a Unix mode and keeps tempfile's
    /// platform default.
    ///
    /// `max_file_bytes` must be in `1..=usize::MAX - 1`. It is a per-file cap,
    /// not a workspace total, file-count, or host disk quota. This crate has
    /// no total-quota API. This does not allocate a buffer of that size and
    /// does not expose the directory path or any host project directory.
    ///
    /// Blocks the calling thread.
    pub fn new(max_file_bytes: usize) -> Result<Self, WorkspaceError> {
        if max_file_bytes == 0 || max_file_bytes.checked_add(1).is_none() {
            return Err(WorkspaceError::InvalidLimit { max_file_bytes });
        }
        let mut builder = tempfile::Builder::new();
        builder.prefix("circus-sandbox-");
        // Mode is passed to mkdir. A later chmod, or a process-wide umask
        // change, would race with other creators.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let temp = builder.tempdir()?;
        let dir = Dir::open_ambient_dir(temp.path(), ambient_authority())?;
        Ok(Self {
            dir: Some(dir),
            temp: Some(temp),
            max_file_bytes,
            file_lock: RwLock::new(()),
        })
    }

    /// Creates `path` and any missing parents inside the workspace.
    ///
    /// Does not create parents for [`Self::write_file`]. Takes the exclusive
    /// file lock after the path check and holds it across the directory
    /// create. Blocks the calling thread.
    pub fn create_dir_all(&self, path: impl AsRef<Path>) -> Result<(), WorkspaceError> {
        self.ensure_open()?;
        let path = check_path(path.as_ref())?;
        let _guard = self.file_lock.write().map_err(lock_poisoned)?;
        let dir = self.dir.as_ref().ok_or_else(missing_handle)?;
        dir.create_dir_all(path)?;
        Ok(())
    }

    /// Writes `contents` to a regular file, overwriting one that already exists.
    ///
    /// Closed is checked first, then the path, then the byte length. The
    /// exclusive file lock is taken only after those checks, so a caller
    /// [`AsRef<Path>`] is not invoked while the lock is held. An over-long
    /// write does not create or truncate. Missing parents are
    /// [`std::io::ErrorKind::NotFound`] and are not created implicitly. A
    /// directory, FIFO, or other non-regular node, including one reached
    /// through an in-workspace symlink, is [`WorkspaceError::NotRegularFile`]
    /// and is left unchanged. On Unix the open is nonblocking, so a regular
    /// file replaced by a FIFO after the path check cannot stall this call.
    ///
    /// The lock covers the final open, `set_len(0)`, and `write_all`. That
    /// makes overlapping calls on this value see a complete payload. It is
    /// not an atomic replace and not crash-safe: an I/O error after the open
    /// can still leave the file truncated or partial. Other processes are not
    /// excluded.
    ///
    /// Blocks the calling thread.
    pub fn write_file(
        &self,
        path: impl AsRef<Path>,
        contents: &[u8],
    ) -> Result<(), WorkspaceError> {
        self.ensure_open()?;
        let path = check_path(path.as_ref())?;
        if contents.len() > self.max_file_bytes {
            return Err(WorkspaceError::LimitExceeded {
                limit: self.max_file_bytes,
                actual: contents.len(),
            });
        }
        let _guard = self.file_lock.write().map_err(lock_poisoned)?;
        let dir = self.dir.as_ref().ok_or_else(missing_handle)?;
        require_writable_node(dir, path)?;
        let file = open_writable(dir, path)?;
        write_regular(file, path, contents)
    }

    /// Reads a regular file, refusing directories and other non-regular nodes.
    ///
    /// An in-workspace symlink is followed. A FIFO target, or a regular file
    /// replaced by a FIFO before the open, is [`WorkspaceError::NotRegularFile`].
    /// On Unix that open is nonblocking and does not wait for a FIFO peer.
    /// The size is not taken from metadata. At most `max_file_bytes + 1` bytes
    /// are read; a longer file returns [`WorkspaceError::LimitExceeded`].
    ///
    /// The shared file lock is taken after the path check and held from the
    /// final open through the end of the read, so a concurrent call on this
    /// value cannot truncate the file mid-read. Other processes are not
    /// excluded. Blocks the calling thread.
    pub fn read_file(&self, path: impl AsRef<Path>) -> Result<Vec<u8>, WorkspaceError> {
        self.ensure_open()?;
        let path = check_path(path.as_ref())?;
        let _guard = self.file_lock.read().map_err(lock_poisoned)?;
        let dir = self.dir.as_ref().ok_or_else(missing_handle)?;
        require_readable_node(dir, path)?;
        let file = open_readable(dir, path)?;
        read_regular(file, path, self.max_file_bytes)
    }

    /// Opens one staged regular file for caller-driven, bounded streaming.
    ///
    /// Returns the length observed on the opened handle and a host file
    /// handle. The handle inherits the same cap-std resolution as
    /// [`Self::read_file`]: the path is checked first, the directory capability
    /// opens it, and the opened handle must be a regular file. The caller reads
    /// at most the returned length in chunks, so no full-file buffer is needed.
    /// The file lock protects lookup and open; an open handle remains pinned to
    /// the inode it resolved, but concurrent external replacement is still
    /// possible, exactly as with `read_file`.
    ///
    /// Blocks the calling thread.
    pub(crate) fn open_staged_file(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<(u64, std::fs::File), WorkspaceError> {
        self.ensure_open()?;
        let path = check_path(path.as_ref())?;
        let _guard = self.file_lock.read().map_err(lock_poisoned)?;
        let dir = self.dir.as_ref().ok_or_else(missing_handle)?;
        require_readable_node(dir, path)?;
        let file = open_readable(dir, path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(not_regular(path));
        }
        let limit = u64::try_from(self.max_file_bytes).unwrap_or(u64::MAX);
        if meta.len() > limit {
            return Err(WorkspaceError::LimitExceeded {
                limit: self.max_file_bytes,
                actual: self.max_file_bytes.saturating_add(1),
            });
        }
        Ok((meta.len(), file.into_std()))
    }

    /// Removes a non-directory node at `path` via cap-std unlink. A symlink is
    /// removed itself and is not followed. Directories are left in place.
    ///
    /// Takes the exclusive file lock after the path check and holds it across
    /// the unlink. Blocks the calling thread.
    pub fn remove_file(&self, path: impl AsRef<Path>) -> Result<(), WorkspaceError> {
        self.ensure_open()?;
        let path = check_path(path.as_ref())?;
        let _guard = self.file_lock.write().map_err(lock_poisoned)?;
        let dir = self.dir.as_ref().ok_or_else(missing_handle)?;
        dir.remove_file(path)?;
        Ok(())
    }

    /// Lists regular files and directories for container staging.
    ///
    /// Each regular file carries its metadata length; payload bytes are not
    /// read. The walk returns parent directories before their children. Uses
    /// this value's capability handle. Symlinks and other non-regular nodes are
    /// errors. Does not return the host root path.
    pub(crate) fn staged_nodes(&self) -> Result<Vec<StagedNode>, WorkspaceError> {
        self.ensure_open()?;
        let _guard = self.file_lock.read().map_err(lock_poisoned)?;
        let dir = self.dir.as_ref().ok_or_else(missing_handle)?;
        let mut out = Vec::new();
        walk_staged(dir, "", &mut out)?;
        Ok(out)
    }

    /// Deletes the owned temporary directory and releases its capability.
    ///
    /// Idempotent. A root that is already gone is success. On failure the same
    /// value keeps the directory so cleanup can be retried, and file operations
    /// do not return [`WorkspaceError::Closed`]. After success, file operations
    /// return `Closed` even for illegal paths.
    ///
    /// Does not take the file lock. `&mut self` already excludes other calls
    /// on this value.
    ///
    /// Blocks the calling thread. Drop remains best-effort and does not report
    /// the error this method returns.
    pub fn cleanup(&mut self) -> Result<(), WorkspaceError> {
        if self.temp.is_none() {
            self.dir = None;
            return Ok(());
        }
        // Release the handle first so Windows can remove the directory.
        self.dir.take();
        let Some(temp) = self.temp.as_ref() else {
            return Ok(());
        };
        match std::fs::remove_dir_all(temp.path()) {
            Ok(()) => {
                self.mark_closed();
                Ok(())
            }
            Err(_) if self.root_is_absent() => {
                self.mark_closed();
                Ok(())
            }
            Err(err) => {
                let _ = self.reopen();
                Err(err.into())
            }
        }
    }

    pub(crate) fn ensure_open(&self) -> Result<(), WorkspaceError> {
        if self.temp.is_none() {
            return Err(WorkspaceError::Closed);
        }
        if self.dir.is_none() {
            return Err(missing_handle());
        }
        Ok(())
    }

    fn mark_closed(&mut self) {
        self.dir = None;
        if let Some(mut temp) = self.temp.take() {
            temp.disable_cleanup(true);
        }
    }

    fn reopen(&mut self) -> io::Result<()> {
        let Some(temp) = self.temp.as_ref() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "workspace directory is gone",
            ));
        };
        let dir = Dir::open_ambient_dir(temp.path(), ambient_authority())?;
        self.dir = Some(dir);
        Ok(())
    }

    fn root_is_absent(&self) -> bool {
        let Some(temp) = self.temp.as_ref() else {
            return true;
        };
        matches!(
            std::fs::symlink_metadata(temp.path()),
            Err(err) if err.kind() == io::ErrorKind::NotFound
        )
    }
}

/// One relative node discovered through the workspace capability handle.
///
/// `len` is the file length observed from metadata during the same walk that
/// discovered the entry. It is a planning value, not a payload copy: the bytes
/// are streamed later through [`Workspace::open_staged_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StagedNode {
    pub relative: String,
    pub is_dir: bool,
    pub len: u64,
}

fn walk_staged(dir: &Dir, prefix: &str, out: &mut Vec<StagedNode>) -> Result<(), WorkspaceError> {
    for entry in dir.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(io::Error::other("workspace entry name is not utf-8").into());
        };
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', ':']) {
            return Err(io::Error::other("workspace entry name is not stageable").into());
        }
        let relative = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(io::Error::other("workspace symlink is not staged").into());
        }
        if file_type.is_dir() {
            out.push(StagedNode {
                relative: relative.clone(),
                is_dir: true,
                len: 0,
            });
            let child = dir.open_dir(name)?;
            walk_staged(&child, &relative, out)?;
        } else if file_type.is_file() {
            let len = entry.metadata()?.len();
            out.push(StagedNode {
                relative,
                is_dir: false,
                len,
            });
        } else {
            return Err(io::Error::other("workspace entry is not a regular file").into());
        }
    }
    Ok(())
}

fn lock_poisoned<T>(err: PoisonError<T>) -> WorkspaceError {
    io::Error::other(format!("workspace file lock poisoned: {err}")).into()
}

fn missing_handle() -> WorkspaceError {
    io::Error::other(
        "workspace directory handle is unavailable after a failed cleanup; retry cleanup",
    )
    .into()
}

fn check_path(path: &Path) -> Result<&Path, WorkspaceError> {
    let raw = path.as_os_str().as_encoded_bytes();
    let reason = if raw.is_empty() {
        Some(PathRejectReason::Empty)
    } else if raw.contains(&b'\\') {
        Some(PathRejectReason::Backslash)
    } else if is_windows_prefix(raw) {
        Some(PathRejectReason::WindowsPrefix)
    } else if raw.starts_with(b"/") || path.is_absolute() {
        Some(PathRejectReason::Absolute)
    } else if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        Some(PathRejectReason::ParentComponent)
    } else if has_windows_drive_component(path) {
        // After `\`, absolute, and `..`, so those existing reasons stay stable.
        Some(PathRejectReason::WindowsPrefix)
    } else {
        None
    };
    match reason {
        Some(reason) => Err(WorkspaceError::PathRejected {
            path: path.to_path_buf(),
            reason,
        }),
        None => Ok(path),
    }
}

fn is_drive_prefix(raw: &[u8]) -> bool {
    raw.len() >= 2 && raw[0].is_ascii_alphabetic() && raw[1] == b':'
}

fn is_windows_prefix(raw: &[u8]) -> bool {
    raw.starts_with(b"//") || is_drive_prefix(raw)
}

fn has_windows_drive_component(path: &Path) -> bool {
    path.components()
        .any(|component| is_drive_prefix(component.as_os_str().as_encoded_bytes()))
}

fn require_readable_node(dir: &Dir, path: &Path) -> Result<(), WorkspaceError> {
    match dir.symlink_metadata(path) {
        Ok(meta) if is_followable(meta.file_type()) => Ok(()),
        Ok(_) => Err(not_regular(path)),
        Err(err) => Err(err.into()),
    }
}

fn require_writable_node(dir: &Dir, path: &Path) -> Result<(), WorkspaceError> {
    match dir.symlink_metadata(path) {
        Ok(meta) if is_followable(meta.file_type()) => Ok(()),
        Ok(_) => Err(not_regular(path)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

// A symlink is not proof of a regular file. In-directory links to regular
// files must still open. This precheck cannot close the window where the
// target is a FIFO, or a regular file is replaced by one before `open`.
fn is_followable(file_type: cap_std::fs::FileType) -> bool {
    file_type.is_file() || file_type.is_symlink()
}

fn read_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    // `OpenOptionsSyncExt::nonblock` is cap-std's open-time `O_NONBLOCK`.
    // A later `fstat` cannot help if `open` already waits for a FIFO peer.
    // Windows keeps its normal CreateFile flags.
    #[cfg(unix)]
    {
        options.nonblock(true);
    }
    options
}

fn write_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        options.nonblock(true);
    }
    options
}

fn open_readable(dir: &Dir, path: &Path) -> Result<File, WorkspaceError> {
    match dir.open_with(path, &read_options()) {
        Ok(file) => Ok(file),
        Err(err) => Err(failed_open(dir, path, err)),
    }
}

fn open_writable(dir: &Dir, path: &Path) -> Result<File, WorkspaceError> {
    match dir.open_with(path, &write_options()) {
        Ok(file) => Ok(file),
        Err(err) => Err(failed_open(dir, path, err)),
    }
}

fn failed_open(dir: &Dir, path: &Path, err: io::Error) -> WorkspaceError {
    if is_dir(dir, path) {
        return not_regular(path);
    }
    // `O_WRONLY|O_NONBLOCK` on a FIFO with no reader fails before a handle
    // exists. Classify with a nonblocking read open, which does not wait for
    // a writer, and keep the regular-file decision on that handle. If that
    // open also fails, `stat` does not wait for a peer and only classifies
    // the already-failed open.
    #[cfg(unix)]
    {
        if let Ok(file) = dir.open_with(path, &read_options()) {
            if matches!(file.metadata(), Ok(meta) if !meta.is_file()) {
                return not_regular(path);
            }
        } else if matches!(dir.metadata(path), Ok(meta) if !meta.is_file()) {
            return not_regular(path);
        }
    }
    err.into()
}

fn is_dir(dir: &Dir, path: &Path) -> bool {
    matches!(dir.metadata(path), Ok(meta) if meta.is_dir())
}

fn not_regular(path: &Path) -> WorkspaceError {
    WorkspaceError::NotRegularFile {
        path: path.to_path_buf(),
    }
}

fn write_regular(mut file: File, path: &Path, contents: &[u8]) -> Result<(), WorkspaceError> {
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(not_regular(path));
    }
    file.set_len(0)?;
    file.write_all(contents)?;
    Ok(())
}

fn read_regular(file: File, path: &Path, limit: usize) -> Result<Vec<u8>, WorkspaceError> {
    let mut buf = Vec::new();
    read_regular_into(file, path, limit, &mut buf)?;
    Ok(buf)
}

fn read_regular_into(
    mut file: File,
    path: &Path,
    limit: usize,
    buf: &mut Vec<u8>,
) -> Result<(), WorkspaceError> {
    buf.clear();
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(not_regular(path));
    }
    let Some(probe) = limit.checked_add(1) else {
        return Err(WorkspaceError::InvalidLimit {
            max_file_bytes: limit,
        });
    };
    let mut chunk = [0u8; 8 * 1024];
    while buf.len() < probe {
        let want = (probe - buf.len()).min(chunk.len());
        let n = file.read(&mut chunk[..want])?;
        if n == 0 {
            break;
        }
        buf.try_reserve(n)
            .map_err(|err| io::Error::new(io::ErrorKind::OutOfMemory, err))?;
        buf.extend_from_slice(&chunk[..n]);
    }
    if buf.len() > limit {
        return Err(WorkspaceError::LimitExceeded {
            limit,
            actual: buf.len(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod snapshot_file_tests;
#[cfg(test)]
mod snapshot_tests;

#[doc(inline)]
pub use code_source::{CodeCopyError, CodeSource};
#[doc(inline)]
pub use git::{GitCheckout, GitCleanup, GitError, GitOptions, GitRevision, GitSource, GitWorktree};
#[doc(inline)]
pub use snapshot::{CodeSnapshot, SnapshotBudget, SnapshotError};
pub(crate) use snapshot::{SnapshotBody, SnapshotEntry};
#[cfg(test)]
mod tests;
