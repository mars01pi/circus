//! Sorted NUL-delimited workspace manifest emitted by the guest helper.
//!
//! The guest writes one record per entry: the kind byte (`d` or `f`) followed
//! by NUL, the UTF-8 workspace-relative path followed by NUL, and for files the
//! decimal byte size followed by NUL. Records are sorted by path bytes, the
//! workspace root is never listed, and the helper refuses to emit more than the
//! caller's byte or entry bound. Decoding here is strict: a truncated record,
//! non-UTF-8 or non-portable path, unknown kind, non-canonical size, duplicate
//! path, or unsorted path is a protocol failure rather than a partial listing.

use super::{EngineError, EngineErrorKind, file_error};

/// Why one listed workspace path exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceEntryKind {
    Directory,
    File,
}

/// One workspace path observed through the confined guest helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceEntry {
    path: String,
    kind: WorkspaceEntryKind,
    size: u64,
}

impl WorkspaceEntry {
    /// Portable workspace-relative path, never a host path.
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn kind(&self) -> WorkspaceEntryKind {
        self.kind
    }

    /// Declared file size; always zero for a directory.
    pub(crate) fn size(&self) -> u64 {
        self.size
    }
}

/// Decodes one complete helper response into strictly ordered entries.
pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<WorkspaceEntry>, EngineError> {
    let mut entries: Vec<WorkspaceEntry> = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let kind = match rest.first().copied() {
            Some(b'd') => WorkspaceEntryKind::Directory,
            Some(b'f') => WorkspaceEntryKind::File,
            _ => return Err(malformed("unknown entry kind")),
        };
        if rest.get(1).copied() != Some(0) {
            return Err(malformed("entry kind is not NUL-terminated"));
        }
        let (raw_path, tail) = field(&rest[2..])?;
        let path = std::str::from_utf8(raw_path).map_err(|_| malformed("path is not UTF-8"))?;
        crate::runtime::check_relative(path).map_err(|_| malformed("path is not portable"))?;
        if entries
            .last()
            .is_some_and(|last| last.path.as_str() >= path)
        {
            return Err(malformed("paths are not strictly sorted"));
        }
        let mut entry = WorkspaceEntry {
            path: path.to_string(),
            kind,
            size: 0,
        };
        rest = match kind {
            WorkspaceEntryKind::Directory => tail,
            WorkspaceEntryKind::File => {
                let (raw_size, tail) = field(tail)?;
                entry.size = decimal(raw_size)?;
                tail
            }
        };
        entries.push(entry);
    }
    Ok(entries)
}

/// Splits one NUL-terminated field, rejecting a truncated record.
fn field(bytes: &[u8]) -> Result<(&[u8], &[u8]), EngineError> {
    match bytes.iter().position(|byte| *byte == 0) {
        Some(end) => Ok((&bytes[..end], &bytes[end + 1..])),
        None => Err(malformed("field is not NUL-terminated")),
    }
}

/// Accepts only the canonical decimal form the helper emits.
fn decimal(raw: &[u8]) -> Result<u64, EngineError> {
    let text = std::str::from_utf8(raw).map_err(|_| malformed("size is not ASCII"))?;
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(malformed("size is not decimal"));
    }
    if text.len() > 1 && text.starts_with('0') {
        return Err(malformed("size has a leading zero"));
    }
    text.parse::<u64>()
        .map_err(|_| malformed("size exceeds 64 bits"))
}

fn malformed(reason: &str) -> EngineError {
    file_error(
        EngineErrorKind::Protocol,
        format!("guest workspace manifest: {reason}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(records: &[&[u8]]) -> Vec<u8> {
        records.concat()
    }

    #[test]
    fn decodes_sorted_files_and_directories() {
        let bytes = entries(&[
            b"d\x00empty\x00",
            b"d\x00nested\x00",
            b"f\x00nested/data.bin\x0012\x00",
            b"f\x00top\x000\x00",
        ]);
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.len(), 4);
        assert_eq!(decoded[0].path(), "empty");
        assert_eq!(decoded[0].kind(), WorkspaceEntryKind::Directory);
        assert_eq!(decoded[0].size(), 0);
        assert_eq!(decoded[2].path(), "nested/data.bin");
        assert_eq!(decoded[2].kind(), WorkspaceEntryKind::File);
        assert_eq!(decoded[2].size(), 12);
        assert_eq!(decoded[3].size(), 0);
    }

    #[test]
    fn accepts_an_empty_workspace() {
        assert!(decode(b"").unwrap().is_empty());
    }

    #[test]
    fn rejects_truncated_records() {
        for bytes in [
            b"d".as_slice(),
            b"d\x00".as_slice(),
            b"d\x00name".as_slice(),
            b"f\x00name\x00".as_slice(),
            b"f\x00name\x0012".as_slice(),
        ] {
            assert!(decode(bytes).is_err(), "accepted {bytes:?}");
        }
    }

    #[test]
    fn rejects_malformed_kinds_paths_and_sizes() {
        for bytes in [
            b"x\x00name\x00".as_slice(),
            b"f\x00\x00".as_slice(),
            b"f\x00/nope\x00".as_slice(),
            b"f\x00a//b\x00".as_slice(),
            b"f\x00dir/../name\x00".as_slice(),
            b"f\x00C:drive\x00".as_slice(),
            b"f\x00back\\slash\x00".as_slice(),
            b"f\x00name\x00-1\x00".as_slice(),
            b"f\x00name\x00+1\x00".as_slice(),
            b"f\x00name\x00 1\x00".as_slice(),
            b"f\x00name\x0007\x00".as_slice(),
            b"f\x00name\x0018446744073709551616\x00".as_slice(),
            b"d\x00name\x001\x00".as_slice(),
        ] {
            assert!(decode(bytes).is_err(), "accepted {bytes:?}");
        }
    }

    #[test]
    fn rejects_duplicates_and_unsorted_paths() {
        for bytes in [
            b"f\x00b\x001\x00f\x00a\x001\x00".as_slice(),
            b"f\x00a\x001\x00f\x00a\x001\x00".as_slice(),
            b"f\x00a/b\x001\x00f\x00a\x001\x00".as_slice(),
        ] {
            assert!(decode(bytes).is_err(), "accepted {bytes:?}");
        }
    }

    #[test]
    fn rejects_non_utf8_paths() {
        assert!(decode(b"f\x00\xff\xfe\x001\x00").is_err());
    }
}
