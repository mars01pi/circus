//! Error normalization shared by the containerd adapter's gRPC calls.

use containerd_client::tonic::{Code, Status};

use super::engine::{EngineError, EngineErrorKind};

/// Whether a failed request may still have changed daemon state.
///
/// Only transport-level outcomes are uncertain: the daemon may have applied the
/// mutation before the connection failed. Typed rejections are definite.
pub(crate) fn status_is_pending(code: Code) -> bool {
    matches!(
        code,
        Code::Unknown
            | Code::Unavailable
            | Code::DeadlineExceeded
            | Code::Cancelled
            | Code::Internal
    )
}

pub(crate) fn map_status(status: Status) -> EngineError {
    let kind = match status.code() {
        Code::NotFound => EngineErrorKind::NotFound,
        Code::PermissionDenied => EngineErrorKind::PermissionDenied,
        Code::ResourceExhausted => EngineErrorKind::Limit,
        _ => EngineErrorKind::Other,
    };
    EngineError {
        message: status.message().to_string(),
        not_found: status.code() == Code::NotFound,
        kind,
        create_pending: status_is_pending(status.code()),
        create_id: None,
        output: None,
        policy: None,
    }
}

/// Normalizes a transport failure that happened before any request was sent.
pub(crate) fn connect_error(error: impl std::fmt::Display) -> EngineError {
    EngineError {
        message: error.to_string(),
        not_found: false,
        kind: EngineErrorKind::Other,
        create_pending: false,
        create_id: None,
        output: None,
        policy: None,
    }
}

/// A definite, locally detected adapter failure (no request was attempted).
pub(crate) fn local_error(message: impl Into<String>) -> EngineError {
    EngineError {
        message: message.into(),
        not_found: false,
        kind: EngineErrorKind::Other,
        create_pending: false,
        create_id: None,
        output: None,
        policy: None,
    }
}

/// Attaches creation identity so the lifecycle owner can attempt recovery.
pub(crate) fn with_ownership(mut error: EngineError, create_id: Option<String>) -> EngineError {
    if create_id.is_some() {
        error.create_id = create_id;
    }
    error
}

/// Clears creation uncertainty for work that never used the instance name.
pub(crate) fn without_ownership(mut error: EngineError) -> EngineError {
    error.create_pending = false;
    error.create_id = None;
    error
}
