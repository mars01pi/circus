//! Confined guest file-helper protocol shared by every backend adapter.
//!
//! The Python helper (`guest_files.py`) is the single implementation of the
//! workspace file protocol and its exit-status contract. Docker, containerd,
//! and Kubernetes adapters only build its argv and map its exit status, so the
//! backends cannot drift apart on path confinement, limits, or error
//! classification.

use super::engine::{EngineError, EngineErrorKind, file_error};

/// Embedded guest helper. Never copied or reimplemented per backend.
pub(crate) const GUEST_FILES: &str = include_str!("guest_files.py");

/// One bounded host-to-guest copy buffer. The guest helper reads stdin in the
/// same 64 KiB units, so neither side needs a whole-file buffer.
pub(crate) const TRANSFER_CHUNK_BYTES: usize = 64 * 1024;

/// Fixed stderr allowance for helper diagnostics.
pub(crate) const HELPER_STDERR_LIMIT: usize = 4096;

/// Stdin framing for one guest exec.
pub(crate) enum GuestStdin<'a> {
    None,
    Bytes(&'a [u8]),
    File { file: std::fs::File, len: u64 },
}

impl GuestStdin<'_> {
    pub(crate) fn attached(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Declared stdin byte count, which the helper uses as its write bound.
    pub(crate) fn bound(&self) -> u64 {
        match self {
            Self::None => 0,
            Self::Bytes(bytes) => u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            Self::File { len, .. } => *len,
        }
    }
}

/// Per-stream allowances for one exec with stdin.
pub(crate) struct ExecIo<'a> {
    pub stdin: GuestStdin<'a>,
    /// Per-stream allowance for stdout.
    pub stdout_limit: usize,
    /// Per-stream allowance for stderr.
    pub stderr_limit: usize,
    /// Combined stdout+stderr budget. Always the explicit sum of the two
    /// allowances, never an accidental doubling.
    pub combined_limit: usize,
}

/// Helper mode. The wire spelling is the helper's argv contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuestFileMode {
    Read,
    Write,
    Mkdir,
    Unlink,
    List,
}

impl GuestFileMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Mkdir => "mkdir",
            Self::Unlink => "unlink",
            Self::List => "list",
        }
    }
}

/// Builds the helper argv for one request.
///
/// `bound` is the caller's payload or read bound: a write uses its declared
/// stdin length, `read` and `list` use the manifest/read byte bound.
pub(crate) fn helper_argv(
    workspace: &str,
    relative: &str,
    mode: GuestFileMode,
    bound: u64,
    max_entries: Option<usize>,
) -> Vec<String> {
    let mut argv = vec![
        "python3".to_string(),
        "-I".to_string(),
        "-B".to_string(),
        "-c".to_string(),
        GUEST_FILES.to_string(),
        mode.as_str().to_string(),
        workspace.to_string(),
        relative.to_string(),
        bound.to_string(),
    ];
    if let Some(max_entries) = max_entries {
        argv.push(max_entries.to_string());
    }
    argv
}

/// Explicit helper budgets: stdout may carry `max_bytes + 1` (the extra byte
/// proves an over-limit file), stderr is fixed, and both share their exact sum.
pub(crate) fn helper_limits(max_bytes: usize) -> (usize, usize, usize) {
    let stdout = max_bytes.saturating_add(1);
    (
        stdout,
        HELPER_STDERR_LIMIT,
        stdout.saturating_add(HELPER_STDERR_LIMIT),
    )
}

/// Maps one non-zero helper exit status to the shared typed error.
pub(crate) fn helper_failure(exit_code: i64) -> EngineError {
    let kind = match exit_code {
        81 => EngineErrorKind::UnsafeFile,
        82 => EngineErrorKind::Limit,
        83 => EngineErrorKind::InvalidPath,
        84 => EngineErrorKind::Protocol,
        85 => EngineErrorKind::FileIo,
        86 => EngineErrorKind::NotFound,
        87 => EngineErrorKind::PermissionDenied,
        88 => EngineErrorKind::EntryLimit,
        _ => EngineErrorKind::Other,
    };
    file_error(
        kind,
        format!("guest file helper failed (exit {exit_code}); Python3 is required"),
    )
}
