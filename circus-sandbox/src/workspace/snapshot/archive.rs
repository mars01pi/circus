//! Bounded, versioned durable encoding of a [`CodeSnapshot`].
//!
//! Format version 1 is a little-endian binary archive:
//!
//! ```text
//! magic        8 bytes  `CRCSNAP\0`
//! version      u16      1
//! flags        u16      0 (reserved, must be zero)
//! entry_count  u64      files plus directories; the root is not listed
//! payload      u64      sum of file-content bytes
//! entry*       sequence in strictly increasing UTF-8 path-byte order:
//!   kind       u8       0 = directory, 1 = regular file
//!   path_len   u32      1..=4096 UTF-8 bytes, `/` separators
//!   path       bytes
//!   file_size  u64      files only, followed by exactly that many bytes
//! ```
//!
//! The archive is self-delimiting: the header count and length bound every
//! allocation, and trailing bytes are rejected. It stores no host path, no
//! permissions, no ownership, no timestamps, and no process or memory state.
//! The caller supplies the host path, owns its storage and retention policy,
//! and supplies the per-file and snapshot budgets on load.

use std::cmp::Ordering;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};

use super::super::WorkspaceError;
use super::{
    Body, CodeSnapshot, Entry, SnapshotBudget, SnapshotError, require_portable, reserve_error,
};

const MAGIC: [u8; 8] = *b"CRCSNAP\0";
const VERSION: u16 = 1;
const HEADER_BYTES: usize = 28;
const KIND_DIR: u8 = 0;
const KIND_FILE: u8 = 1;
/// Format limit for one portable UTF-8 path.
const MAX_PATH_BYTES: usize = 4096;
/// Smallest encoded entry: kind, path length, and one path byte.
const MIN_ENTRY_BYTES: u64 = 6;
/// Initial entry allocation; parsing grows it incrementally so a forged count
/// cannot reserve memory from the header alone.
const PREALLOC_ENTRIES: u64 = 1024;

impl CodeSnapshot {
    /// Atomically publishes the snapshot to a caller-supplied host path.
    ///
    /// The bytes are written to a temporary file in the target's parent
    /// directory, flushed and synced, then renamed over `path`. A successful
    /// rename replaces the directory entry, so saving over a symlink replaces
    /// the link itself rather than writing through it. A failed save removes its
    /// temporary file; a temporary file left by a hard process crash is not an
    /// archive and is never read automatically.
    ///
    /// `path` is trusted caller input; this method does not choose a default
    /// location, create directories, lock against other writers, encrypt the
    /// archive, or change the temporary file's creation permissions. Every
    /// stored path must be a canonical `/`-separated UTF-8 relative path;
    /// non-UTF-8, non-canonical, duplicate, or non-portable paths are rejected
    /// before a temporary file is created. The format stores regular files and
    /// directories; it is not a process or memory checkpoint, not a quota, and
    /// has no encryption, retention, or tenant enforcement.
    ///
    /// Blocks the calling thread.
    pub fn save_file(&self, path: &Path) -> Result<(), SnapshotError> {
        let mut ordered = Vec::new();
        ordered
            .try_reserve(self.entries.len())
            .map_err(reserve_error)?;

        let mut total = 0u64;
        for entry in &self.entries {
            let text = save_path(&entry.path)?;
            if let Body::File(bytes) = &entry.body {
                if bytes.len() > self.max_file_bytes {
                    return Err(limit_error(self.max_file_bytes, bytes.len()));
                }
                let size = u64::try_from(bytes.len())
                    .map_err(|_| invalid("snapshot file is too large to encode"))?;
                total = total
                    .checked_add(size)
                    .ok_or_else(|| invalid("snapshot byte total overflowed"))?;
            }
            ordered.push((text, entry));
        }

        ordered.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        let mut previous: Option<&str> = None;
        for (text, _) in &ordered {
            if previous == Some(text.as_str()) {
                return Err(invalid("snapshot contains a duplicate path"));
            }
            previous = Some(text.as_str());
        }

        let entry_count = u64::try_from(ordered.len())
            .map_err(|_| invalid("snapshot entry count is too large to encode"))?;
        let parent = parent_directory(path);
        let mut temporary = tempfile::Builder::new()
            .prefix(".circus-snapshot-")
            .suffix(".tmp")
            .tempfile_in(parent)
            .map_err(|err| io_error("create temporary file", err))?;
        {
            let mut writer = BufWriter::new(temporary.as_file_mut());
            write_header(&mut writer, entry_count, total)?;
            for (text, entry) in &ordered {
                write_entry(&mut writer, text.as_str(), entry)?;
            }
            writer.flush().map_err(|err| io_error("write", err))?;
        }
        temporary
            .as_file()
            .sync_all()
            .map_err(|err| io_error("sync", err))?;
        temporary
            .persist(path)
            .map_err(|error| io_error("rename", error.error))?;
        Ok(())
    }

    /// Reads a snapshot archive with caller-supplied per-file and snapshot
    /// budgets.
    ///
    /// `max_file_bytes` is the per-file cap used by the returned value and
    /// [`Self::restore`]; it must be in `1..usize::MAX`. Every encoded file must
    /// fit it. `budget` bounds the encoded entry count, each file's payload,
    /// and the total payload before allocation. Header lengths are never
    /// trusted for allocation beyond the archive's actual remaining bytes,
    /// the 4096-byte path limit, and the caller's budgets.
    ///
    /// The archive path is inspected with [`std::fs::symlink_metadata`]; a
    /// symlink, directory, or other non-regular path is rejected without being
    /// followed. Malformed input — bad magic, unsupported version or flags,
    /// unknown kind, truncated record, trailing bytes, duplicate or unsorted
    /// paths, non-UTF-8 or non-portable paths, and oversized path, payload, or
    /// count — returns [`SnapshotError::InvalidArchive`] or the matching budget
    /// error. Host open, read, and metadata failures return
    /// [`SnapshotError::Io`] with no path.
    ///
    /// Blocks the calling thread. The returned value is not a process or
    /// memory checkpoint, and the caller still owns the archive's storage,
    /// retention, encryption, and tenant scope.
    pub fn load_file(
        path: &Path,
        max_file_bytes: usize,
        budget: SnapshotBudget,
    ) -> Result<Self, SnapshotError> {
        if max_file_bytes == 0 || max_file_bytes == usize::MAX {
            return Err(SnapshotError::InvalidArchiveLimit);
        }
        let metadata = std::fs::symlink_metadata(path).map_err(|err| io_error("inspect", err))?;
        if metadata.file_type().is_symlink() {
            return Err(refused(
                "refusing to load a snapshot archive through a symlink",
            ));
        }
        if !metadata.is_file() {
            return Err(refused("snapshot archive path is not a regular file"));
        }
        let file = std::fs::File::open(path).map_err(|err| io_error("open", err))?;
        let opened = file.metadata().map_err(|err| io_error("inspect", err))?;
        if !opened.is_file() {
            return Err(refused("snapshot archive is not a regular file"));
        }
        let mut remaining = opened.len();
        let mut reader = BufReader::new(file);

        let mut header = [0u8; HEADER_BYTES];
        read_exact_bounded(&mut reader, &mut remaining, &mut header)?;
        if !header.starts_with(&MAGIC) {
            return Err(invalid("snapshot archive magic does not match"));
        }
        let version = u16::from_le_bytes(
            header[8..10]
                .try_into()
                .map_err(|_| invalid("snapshot header is truncated"))?,
        );
        if version != VERSION {
            return Err(invalid("snapshot archive version is unsupported"));
        }
        let flags = u16::from_le_bytes(
            header[10..12]
                .try_into()
                .map_err(|_| invalid("snapshot header is truncated"))?,
        );
        if flags != 0 {
            return Err(invalid("snapshot archive flags are unsupported"));
        }
        let entry_count = u64::from_le_bytes(
            header[12..20]
                .try_into()
                .map_err(|_| invalid("snapshot header is truncated"))?,
        );
        let declared_total = u64::from_le_bytes(
            header[20..28]
                .try_into()
                .map_err(|_| invalid("snapshot header is truncated"))?,
        );

        let entry_limit = u64::try_from(budget.max_entries()).unwrap_or(u64::MAX);
        if entry_count > entry_limit {
            return Err(SnapshotError::EntryLimit {
                limit: budget.max_entries(),
                actual: usize::try_from(entry_count).unwrap_or(usize::MAX),
            });
        }
        let total_limit = u64::try_from(budget.max_total_bytes()).unwrap_or(u64::MAX);
        if declared_total > total_limit {
            return Err(SnapshotError::ByteLimit {
                limit: budget.max_total_bytes(),
                actual: usize::try_from(declared_total).unwrap_or(usize::MAX),
            });
        }
        if declared_total > remaining {
            return Err(invalid("snapshot archive is truncated"));
        }
        if entry_count > remaining / MIN_ENTRY_BYTES {
            return Err(invalid("snapshot entry count exceeds the archive length"));
        }

        let mut entries = Vec::new();
        let reserve = usize::try_from(entry_count.min(PREALLOC_ENTRIES))
            .map_err(|_| invalid("snapshot entry count is too large"))?;
        entries.try_reserve(reserve).map_err(reserve_error)?;
        let mut total = 0u64;
        let mut previous: Option<String> = None;

        for _ in 0..entry_count {
            let kind = read_u8(&mut reader, &mut remaining)?;
            if kind != KIND_DIR && kind != KIND_FILE {
                return Err(invalid("snapshot archive has an unknown entry kind"));
            }
            let path_len = u64::from(read_u32(&mut reader, &mut remaining)?);
            if path_len == 0 || path_len > MAX_PATH_BYTES as u64 {
                return Err(invalid("snapshot path length is outside the format limit"));
            }
            let path_len =
                usize::try_from(path_len).map_err(|_| invalid("snapshot path is too long"))?;
            let mut raw_path = Vec::new();
            raw_path
                .try_reserve_exact(path_len)
                .map_err(reserve_error)?;
            raw_path.resize(path_len, 0);
            read_exact_bounded(&mut reader, &mut remaining, &mut raw_path)?;
            let text = std::str::from_utf8(&raw_path)
                .map_err(|_| invalid("snapshot path is not valid UTF-8"))?;
            let path = Path::new(text);
            let canonical = canonical_portable_text(path)
                .map_err(|_| invalid("snapshot path is not portable"))?;
            if canonical.as_str() != text {
                return Err(invalid("snapshot path is not canonical"));
            }
            if let Some(prior) = &previous {
                match prior.as_bytes().cmp(canonical.as_bytes()) {
                    Ordering::Equal => {
                        return Err(invalid("snapshot archive contains a duplicate path"));
                    }
                    Ordering::Greater => {
                        return Err(invalid("snapshot archive paths are not in canonical order"));
                    }
                    Ordering::Less => {}
                }
            }
            previous = Some(canonical.clone());

            let body = match kind {
                KIND_DIR => Body::Dir,
                KIND_FILE => {
                    let declared_size = read_u64(&mut reader, &mut remaining)?;
                    let size = usize::try_from(declared_size)
                        .map_err(|_| limit_error(max_file_bytes, usize::MAX))?;
                    if size > max_file_bytes {
                        return Err(limit_error(max_file_bytes, size));
                    }
                    if declared_size > remaining {
                        return Err(invalid("snapshot archive is truncated"));
                    }
                    total = total
                        .checked_add(declared_size)
                        .ok_or_else(|| invalid("snapshot byte total overflowed"))?;
                    if total > total_limit {
                        return Err(SnapshotError::ByteLimit {
                            limit: budget.max_total_bytes(),
                            actual: usize::try_from(total).unwrap_or(usize::MAX),
                        });
                    }
                    if total > declared_total {
                        return Err(invalid("snapshot file payloads exceed the declared total"));
                    }
                    let mut bytes = Vec::new();
                    bytes.try_reserve_exact(size).map_err(reserve_error)?;
                    bytes.resize(size, 0);
                    read_exact_bounded(&mut reader, &mut remaining, &mut bytes)?;
                    Body::File(bytes)
                }
                _ => unreachable!("kind was checked above"),
            };
            entries.try_reserve(1).map_err(reserve_error)?;
            entries.push(Entry {
                path: PathBuf::from(canonical),
                body,
            });
        }

        if total != declared_total {
            return Err(invalid("snapshot byte total does not match the entries"));
        }
        if remaining != 0 {
            return Err(invalid("snapshot archive has trailing bytes"));
        }
        Ok(Self {
            max_file_bytes,
            total_bytes: usize::try_from(total)
                .map_err(|_| invalid("snapshot byte total is too large"))?,
            entries,
        })
    }
}

fn save_path(path: &Path) -> Result<String, SnapshotError> {
    let canonical = canonical_portable_text(path)?;
    let text = path.to_str().ok_or_else(|| SnapshotError::NotPortable {
        path: path.to_path_buf(),
    })?;
    if canonical.as_str() != text {
        return Err(invalid("snapshot path is not canonical"));
    }
    Ok(canonical)
}

/// Validates one workspace-relative path and returns its canonical `/`-joined
/// UTF-8 form.
fn canonical_portable_text(path: &Path) -> Result<String, SnapshotError> {
    require_portable(path).map_err(|_| SnapshotError::NotPortable {
        path: path.to_path_buf(),
    })?;
    let mut text = String::new();
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(SnapshotError::NotPortable {
                path: path.to_path_buf(),
            });
        };
        let name = name.to_str().ok_or_else(|| SnapshotError::NotPortable {
            path: path.to_path_buf(),
        })?;
        if !text.is_empty() {
            text.push('/');
        }
        text.push_str(name);
    }
    if text.is_empty() || text.len() > MAX_PATH_BYTES {
        return Err(invalid("snapshot path length is outside the format limit"));
    }
    Ok(text)
}

fn parent_directory(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn write_header<W: Write>(
    writer: &mut W,
    entry_count: u64,
    total_bytes: u64,
) -> Result<(), SnapshotError> {
    write_bytes(writer, &MAGIC)?;
    write_bytes(writer, &VERSION.to_le_bytes())?;
    write_bytes(writer, &0u16.to_le_bytes())?;
    write_bytes(writer, &entry_count.to_le_bytes())?;
    write_bytes(writer, &total_bytes.to_le_bytes())?;
    Ok(())
}

fn write_entry<W: Write>(writer: &mut W, text: &str, entry: &Entry) -> Result<(), SnapshotError> {
    let path_len = u32::try_from(text.len())
        .map_err(|_| invalid("snapshot path length is outside the format limit"))?;
    let kind = match &entry.body {
        Body::Dir => KIND_DIR,
        Body::File(_) => KIND_FILE,
    };
    write_bytes(writer, &[kind])?;
    write_bytes(writer, &path_len.to_le_bytes())?;
    write_bytes(writer, text.as_bytes())?;
    if let Body::File(bytes) = &entry.body {
        let size = u64::try_from(bytes.len())
            .map_err(|_| invalid("snapshot file is too large to encode"))?;
        write_bytes(writer, &size.to_le_bytes())?;
        write_bytes(writer, bytes)?;
    }
    Ok(())
}

fn write_bytes<W: Write>(writer: &mut W, bytes: &[u8]) -> Result<(), SnapshotError> {
    writer
        .write_all(bytes)
        .map_err(|err| io_error("write", err))
}

fn read_exact_bounded<R: Read>(
    reader: &mut R,
    remaining: &mut u64,
    buffer: &mut [u8],
) -> Result<(), SnapshotError> {
    let needed = u64::try_from(buffer.len()).map_err(|_| invalid("snapshot field is too large"))?;
    if needed > *remaining {
        return Err(invalid("snapshot archive is truncated"));
    }
    reader.read_exact(buffer).map_err(|err| {
        if err.kind() == io::ErrorKind::UnexpectedEof {
            invalid("snapshot archive is truncated")
        } else {
            io_error("read", err)
        }
    })?;
    *remaining -= needed;
    Ok(())
}

fn read_u8<R: Read>(reader: &mut R, remaining: &mut u64) -> Result<u8, SnapshotError> {
    let mut bytes = [0u8; 1];
    read_exact_bounded(reader, remaining, &mut bytes)?;
    Ok(bytes[0])
}

fn read_u32<R: Read>(reader: &mut R, remaining: &mut u64) -> Result<u32, SnapshotError> {
    let mut bytes = [0u8; 4];
    read_exact_bounded(reader, remaining, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64<R: Read>(reader: &mut R, remaining: &mut u64) -> Result<u64, SnapshotError> {
    let mut bytes = [0u8; 8];
    read_exact_bounded(reader, remaining, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn io_error(operation: &'static str, source: io::Error) -> SnapshotError {
    SnapshotError::Io { operation, source }
}

fn refused(reason: &'static str) -> SnapshotError {
    io_error(
        "inspect",
        io::Error::new(io::ErrorKind::InvalidInput, reason),
    )
}

fn invalid(reason: &'static str) -> SnapshotError {
    SnapshotError::InvalidArchive {
        reason: reason.to_owned(),
    }
}

fn limit_error(limit: usize, actual: usize) -> SnapshotError {
    SnapshotError::Workspace(WorkspaceError::LimitExceeded { limit, actual })
}
