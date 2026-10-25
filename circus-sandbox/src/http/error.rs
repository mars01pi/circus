//! Stable HTTP error bodies and neutral domain-error mapping.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::api::{SandboxError, StagingError};
use crate::workspace::{SnapshotError, WorkspaceError};

/// Stable JSON error body. Codes are part of the HTTP contract.
#[derive(Debug, Serialize)]
pub(super) struct ErrorBody {
    pub(super) code: &'static str,
    pub(super) message: &'static str,
}

/// One HTTP failure. The message is fixed per code and never derived from an
/// error's `Display`, so backend text and SDK types cannot cross the boundary.
#[derive(Debug, Clone, Copy)]
pub(super) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl ApiError {
    const fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }

    pub(super) const fn invalid_request() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "request is invalid",
        )
    }

    pub(super) const fn invalid_json() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "request body is not valid JSON for this endpoint",
        )
    }

    pub(super) const fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "a valid bearer token is required",
        )
    }

    pub(super) const fn forbidden() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "the tenant is not allowed to use this service",
        )
    }

    pub(super) const fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "resource not found")
    }

    pub(super) const fn method_not_allowed() -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "method not allowed for this resource",
        )
    }

    pub(super) const fn conflict() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "conflict",
            "the sandbox is not in a phase that allows this operation",
        )
    }

    pub(super) const fn limit_exceeded() -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "limit_exceeded",
            "the request exceeds a configured limit",
        )
    }

    pub(super) const fn cancelled() -> Self {
        Self::new(
            StatusCode::REQUEST_TIMEOUT,
            "cancelled",
            "the request was cancelled",
        )
    }

    pub(super) const fn timeout() -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "timeout",
            "the operation timed out",
        )
    }

    pub(super) const fn cleanup_required() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "cleanup_required",
            "sandbox cleanup is not confirmed; retry DELETE",
        )
    }

    pub(super) const fn backend_unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "backend_unavailable",
            "the sandbox backend is unavailable",
        )
    }

    pub(super) const fn unsupported_backend() -> Self {
        Self::new(
            StatusCode::NOT_IMPLEMENTED,
            "unsupported_backend",
            "the requested backend or capability is not implemented",
        )
    }

    pub(super) const fn internal_error() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "the request could not be completed",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                code: self.code,
                message: self.message,
            }),
        )
            .into_response()
    }
}

pub(super) fn from_sandbox(error: SandboxError) -> ApiError {
    match error {
        SandboxError::InvalidProcessRequest { .. }
        | SandboxError::Command(_)
        | SandboxError::Config(_)
        | SandboxError::BackendConfig(_)
        | SandboxError::RelativePath { .. } => ApiError::invalid_request(),
        SandboxError::ProcessActive | SandboxError::InvalidPhase { .. } => ApiError::conflict(),
        SandboxError::Workspace(error) => from_workspace(error),
        SandboxError::Snapshot(error) => from_snapshot(error),
        SandboxError::SnapshotInterrupted { interruption, .. } => from_sandbox(*interruption),
        SandboxError::Staging(StagingError::LimitExceeded { .. })
        | SandboxError::Staging(StagingError::SizeOverflow { .. }) => ApiError::limit_exceeded(),
        SandboxError::BackendImageUnavailable { .. }
        | SandboxError::Backend { .. }
        | SandboxError::WorkerGone { .. } => ApiError::backend_unavailable(),
        SandboxError::TimedOut { .. } => ApiError::timeout(),
        SandboxError::Cancelled { .. } => ApiError::cancelled(),
        SandboxError::OutputLimit { .. } => ApiError::limit_exceeded(),
        SandboxError::PolicyMismatch { .. } => ApiError::internal_error(),
        SandboxError::CreationCleanupRequired { .. } | SandboxError::CleanupRequired { .. } => {
            ApiError::cleanup_required()
        }
        SandboxError::UnsupportedCapability { .. } => ApiError::unsupported_backend(),
    }
}

fn from_workspace(error: WorkspaceError) -> ApiError {
    match error {
        WorkspaceError::LimitExceeded { .. } => ApiError::limit_exceeded(),
        WorkspaceError::Closed => ApiError::conflict(),
        WorkspaceError::InvalidLimit { .. }
        | WorkspaceError::PathRejected { .. }
        | WorkspaceError::NotRegularFile { .. } => ApiError::invalid_request(),
        WorkspaceError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ApiError::not_found()
        }
        WorkspaceError::Io(_) => ApiError::internal_error(),
    }
}

fn from_snapshot(error: SnapshotError) -> ApiError {
    match error {
        SnapshotError::EntryLimit { .. } | SnapshotError::ByteLimit { .. } => {
            ApiError::limit_exceeded()
        }
        SnapshotError::InvalidArchive { .. }
        | SnapshotError::InvalidArchiveLimit
        | SnapshotError::InvalidBudget { .. }
        | SnapshotError::Symlink { .. }
        | SnapshotError::NotRegular { .. }
        | SnapshotError::NotPortable { .. } => ApiError::invalid_request(),
        SnapshotError::Workspace(error) => from_workspace(error),
        SnapshotError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            ApiError::not_found()
        }
        SnapshotError::Io { .. } => ApiError::internal_error(),
        SnapshotError::RestoreCleanup { .. } => ApiError::cleanup_required(),
        SnapshotError::Interrupted => ApiError::cancelled(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::path::PathBuf;

    use crate::api::{
        CleanupFailure, CommandError, OutputPhase, Ownership, SandboxError, SandboxOperation,
        SandboxPhase, StagingError,
    };
    use crate::runtime::{BackendConfigError, ConfigError};
    use crate::workspace::{PathRejectReason, SnapshotError, Workspace, WorkspaceError};

    fn assert_maps(error: SandboxError, status: StatusCode, code: &str) {
        let mapped = from_sandbox(error);
        assert_eq!(mapped.code, code);
        assert_eq!(mapped.into_response().status(), status);
    }

    #[test]
    fn stable_api_errors_keep_fixed_codes() {
        for (error, status, code) in [
            (
                ApiError::invalid_request(),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                ApiError::invalid_json(),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                ApiError::unauthorized(),
                StatusCode::UNAUTHORIZED,
                "unauthorized",
            ),
            (ApiError::forbidden(), StatusCode::FORBIDDEN, "forbidden"),
            (ApiError::not_found(), StatusCode::NOT_FOUND, "not_found"),
            (
                ApiError::method_not_allowed(),
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
            ),
            (ApiError::conflict(), StatusCode::CONFLICT, "conflict"),
            (
                ApiError::limit_exceeded(),
                StatusCode::PAYLOAD_TOO_LARGE,
                "limit_exceeded",
            ),
            (
                ApiError::cancelled(),
                StatusCode::REQUEST_TIMEOUT,
                "cancelled",
            ),
            (ApiError::timeout(), StatusCode::GATEWAY_TIMEOUT, "timeout"),
            (
                ApiError::cleanup_required(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "cleanup_required",
            ),
            (
                ApiError::backend_unavailable(),
                StatusCode::SERVICE_UNAVAILABLE,
                "backend_unavailable",
            ),
            (
                ApiError::unsupported_backend(),
                StatusCode::NOT_IMPLEMENTED,
                "unsupported_backend",
            ),
            (
                ApiError::internal_error(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
            ),
        ] {
            let response = error.into_response();
            assert_eq!(response.status(), status);
            assert_eq!(error.code, code);
            assert!(!error.message.is_empty());
        }
    }

    #[test]
    fn sandbox_errors_map_to_stable_http_codes() {
        assert_maps(
            SandboxError::InvalidProcessRequest { reason: "bad" },
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
        assert_maps(
            SandboxError::Command(CommandError::EmptyArgv),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
        assert_maps(
            SandboxError::Config(ConfigError::InvalidWorkspacePath),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
        assert_maps(
            SandboxError::BackendConfig(BackendConfigError::ImageRequired),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
        assert_maps(
            SandboxError::RelativePath { reason: "absolute" },
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
        assert_maps(
            SandboxError::ProcessActive,
            StatusCode::CONFLICT,
            "conflict",
        );
        assert_maps(
            SandboxError::InvalidPhase {
                current: SandboxPhase::Destroyed,
                operation: "start",
            },
            StatusCode::CONFLICT,
            "conflict",
        );
        assert_maps(
            SandboxError::Staging(StagingError::LimitExceeded {
                limit: 1,
                actual: 2,
            }),
            StatusCode::PAYLOAD_TOO_LARGE,
            "limit_exceeded",
        );
        assert_maps(
            SandboxError::Staging(StagingError::SizeOverflow {
                accumulated: 1,
                next: u64::MAX,
                limit: 1,
            }),
            StatusCode::PAYLOAD_TOO_LARGE,
            "limit_exceeded",
        );
        assert_maps(
            SandboxError::BackendImageUnavailable {
                image: "img".into(),
            },
            StatusCode::SERVICE_UNAVAILABLE,
            "backend_unavailable",
        );
        assert_maps(
            SandboxError::Backend {
                message: "down".into(),
            },
            StatusCode::SERVICE_UNAVAILABLE,
            "backend_unavailable",
        );
        assert_maps(
            SandboxError::WorkerGone {
                detail: "gone".into(),
            },
            StatusCode::SERVICE_UNAVAILABLE,
            "backend_unavailable",
        );
        assert_maps(
            SandboxError::TimedOut {
                instance_id: None,
                ownership: Ownership::Unknown,
                operation: SandboxOperation::Exec,
                detail: "late".into(),
            },
            StatusCode::GATEWAY_TIMEOUT,
            "timeout",
        );
        assert_maps(
            SandboxError::Cancelled {
                instance_id: None,
                ownership: Ownership::Unknown,
                operation: SandboxOperation::Exec,
                detail: "stop".into(),
            },
            StatusCode::REQUEST_TIMEOUT,
            "cancelled",
        );
        assert_maps(
            SandboxError::OutputLimit {
                limit: 8,
                phase: OutputPhase::Stdout,
                ownership: Ownership::ConfirmedAbsent,
                detail: "cap".into(),
            },
            StatusCode::PAYLOAD_TOO_LARGE,
            "limit_exceeded",
        );
        assert_maps(
            SandboxError::PolicyMismatch {
                field: "network_mode".into(),
                expected: "none".into(),
                actual: "bridge".into(),
            },
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
        );
        assert_maps(
            SandboxError::CleanupRequired {
                instance_id: "id".into(),
                ownership: Ownership::StillPresent,
                detail: "retry".into(),
                failure: CleanupFailure::Backend {
                    message: "rm".into(),
                },
            },
            StatusCode::INTERNAL_SERVER_ERROR,
            "cleanup_required",
        );
        assert_maps(
            SandboxError::UnsupportedCapability {
                capability: "pause",
            },
            StatusCode::NOT_IMPLEMENTED,
            "unsupported_backend",
        );
        assert_maps(
            SandboxError::SnapshotInterrupted {
                interruption: Box::new(SandboxError::TimedOut {
                    instance_id: None,
                    ownership: Ownership::Unknown,
                    operation: SandboxOperation::CaptureWorkspaceSnapshot,
                    detail: "late".into(),
                }),
                snapshot: Box::new(SnapshotError::Interrupted),
            },
            StatusCode::GATEWAY_TIMEOUT,
            "timeout",
        );
    }

    #[test]
    fn workspace_and_snapshot_errors_map_through_sandbox() {
        assert_maps(
            SandboxError::Workspace(WorkspaceError::LimitExceeded {
                limit: 1,
                actual: 2,
            }),
            StatusCode::PAYLOAD_TOO_LARGE,
            "limit_exceeded",
        );
        assert_maps(
            SandboxError::Workspace(WorkspaceError::Closed),
            StatusCode::CONFLICT,
            "conflict",
        );
        assert_maps(
            SandboxError::Workspace(WorkspaceError::InvalidLimit { max_file_bytes: 0 }),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
        assert_maps(
            SandboxError::Workspace(WorkspaceError::PathRejected {
                path: PathBuf::from(".."),
                reason: PathRejectReason::ParentComponent,
            }),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
        assert_maps(
            SandboxError::Workspace(WorkspaceError::NotRegularFile {
                path: PathBuf::from("fifo"),
            }),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
        assert_maps(
            SandboxError::Workspace(WorkspaceError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "missing",
            ))),
            StatusCode::NOT_FOUND,
            "not_found",
        );
        assert_maps(
            SandboxError::Workspace(WorkspaceError::Io(io::Error::other("io"))),
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
        );
        assert_maps(
            SandboxError::Snapshot(SnapshotError::EntryLimit {
                limit: 1,
                actual: 2,
            }),
            StatusCode::PAYLOAD_TOO_LARGE,
            "limit_exceeded",
        );
        assert_maps(
            SandboxError::Snapshot(SnapshotError::ByteLimit {
                limit: 1,
                actual: 2,
            }),
            StatusCode::PAYLOAD_TOO_LARGE,
            "limit_exceeded",
        );
        assert_maps(
            SandboxError::Snapshot(SnapshotError::InvalidArchive {
                reason: "bad".into(),
            }),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
        assert_maps(
            SandboxError::Snapshot(SnapshotError::InvalidArchiveLimit),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
        assert_maps(
            SandboxError::Snapshot(SnapshotError::InvalidBudget {
                max_total_bytes: 0,
                max_entries: 0,
            }),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
        assert_maps(
            SandboxError::Snapshot(SnapshotError::Symlink {
                path: PathBuf::from("link"),
            }),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
        assert_maps(
            SandboxError::Snapshot(SnapshotError::NotRegular {
                path: PathBuf::from("dev"),
            }),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
        assert_maps(
            SandboxError::Snapshot(SnapshotError::NotPortable {
                path: PathBuf::from("bad name"),
            }),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        );
        assert_maps(
            SandboxError::Snapshot(SnapshotError::Workspace(WorkspaceError::Closed)),
            StatusCode::CONFLICT,
            "conflict",
        );
        assert_maps(
            SandboxError::Snapshot(SnapshotError::Io {
                operation: "open",
                source: io::Error::new(io::ErrorKind::NotFound, "missing"),
            }),
            StatusCode::NOT_FOUND,
            "not_found",
        );
        assert_maps(
            SandboxError::Snapshot(SnapshotError::Io {
                operation: "write",
                source: io::Error::other("io"),
            }),
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
        );
        assert_maps(
            SandboxError::Snapshot(SnapshotError::RestoreCleanup {
                restore: Box::new(SnapshotError::Interrupted),
                destination: Box::new(Workspace::new(64).expect("workspace")),
                cleanup: WorkspaceError::Closed,
            }),
            StatusCode::INTERNAL_SERVER_ERROR,
            "cleanup_required",
        );
        assert_maps(
            SandboxError::Snapshot(SnapshotError::Interrupted),
            StatusCode::REQUEST_TIMEOUT,
            "cancelled",
        );
    }
}
