//! Generic lifecycle state operations, independent of any backend SDK.

use std::path::Path;
use std::sync::Arc;

use crate::infrastructure::{
    Backend, CommonSpec, EngineError, EngineErrorKind, InstanceId,
    OutputPhase as EngineOutputPhase, Recovered, UploadEntry, UploadSource,
};
use crate::observability::events;
use crate::runtime::{SandboxConfig, check_relative};
use crate::workspace::WorkspaceError;

use super::staging::{self, TransferFailure};

use super::{
    CleanupFailure, CleanupReport, CommandOutput, CommandRequest, EnforcementReport, OutputPhase,
    Ownership, SandboxError, SandboxOperation, SandboxPhase, SandboxStatus, State,
};

/// Builds backend-neutral create values once per sandbox.
pub(super) fn common_spec(config: &SandboxConfig) -> CommonSpec {
    let limits = config.limits();
    CommonSpec {
        memory: i64::try_from(limits.memory_bytes()).unwrap_or(i64::MAX),
        nano_cpus: i64::from(limits.cpu_millis()) * 1_000_000,
        pids: i64::from(limits.pids()),
        workspace_path: config.isolation().workspace_path().to_string(),
        workspace_bytes: limits.workspace_bytes(),
        guest_uid: config.isolation().guest_uid(),
        guest_gid: config.isolation().guest_gid(),
    }
}

/// Rejects a configuration whose backend cannot transfer guest files.
///
/// Creation only needs create/exec/file transport. Every other operation
/// negotiates its own capability when it is requested, so a backend without
/// pause/resume, snapshots, or interactive processes can still create and run
/// a sandbox.
pub(super) fn require_capabilities<B: Backend>(backend: &B) -> Result<(), SandboxError> {
    if !backend.capabilities().file_transport {
        return Err(SandboxError::UnsupportedCapability {
            capability: "transfer guest files",
        });
    }
    Ok(())
}

/// Rejects one operation the selected backend cannot perform.
pub(super) fn require_capability(
    supported: bool,
    capability: &'static str,
) -> Result<(), SandboxError> {
    if supported {
        Ok(())
    } else {
        Err(SandboxError::UnsupportedCapability { capability })
    }
}

pub(super) async fn start<B: Backend>(state: &mut State<B>) -> Result<(), SandboxError> {
    need(state, SandboxPhase::Created, "start")?;
    // Validate and count every staged node from metadata before the instance
    // can run. Payloads are not read here and no aggregate buffer is built.
    let plan = staging::plan(state).await?;
    let id = required_id(state)?;
    if let Err(err) = state.backend.start(&id).await {
        return cleanup_engine_error(state, err).await;
    }
    if let Err(failure) = staging::transfer(state, &id, &plan).await {
        return cleanup_transfer_failure(state, failure).await;
    }
    state.phase = SandboxPhase::Running;
    Ok(())
}

/// Destroys an instance whose transfer failed after `start` was accepted, then
/// reports the typed transfer failure. A failed cleanup keeps ownership and is
/// never rewritten as success.
async fn cleanup_transfer_failure<B: Backend, T>(
    state: &mut State<B>,
    failure: TransferFailure,
) -> Result<T, SandboxError> {
    match failure {
        TransferFailure::Engine(err) => cleanup_engine_error(state, err).await,
        TransferFailure::Workspace(err) => {
            let report = destroy_report(state).await;
            if state.phase == SandboxPhase::CleanupRequired {
                return Err(SandboxError::CleanupRequired {
                    instance_id: report.instance_id.unwrap_or_else(|| state.name.clone()),
                    ownership: report.ownership,
                    detail: format!("{err}; {}", report.detail),
                    failure: CleanupFailure::Staging(err),
                });
            }
            Err(err.into())
        }
        TransferFailure::Worker(err) => {
            let message = err.to_string();
            let report = destroy_report(state).await;
            if state.phase == SandboxPhase::CleanupRequired {
                return Err(SandboxError::CleanupRequired {
                    instance_id: report.instance_id.unwrap_or_else(|| state.name.clone()),
                    ownership: report.ownership,
                    detail: format!("{message}; {}", report.detail),
                    failure: CleanupFailure::Backend { message },
                });
            }
            Err(err)
        }
    }
}

pub(super) async fn pause<B: Backend>(state: &mut State<B>) -> Result<(), SandboxError> {
    require_capability(
        state.backend.capabilities().pause_resume,
        "pause and resume",
    )?;
    need(state, SandboxPhase::Running, "pause")?;
    let id = required_id(state)?;
    if let Err(err) = state.backend.pause(&id).await {
        return cleanup_engine_error(state, err).await;
    }
    state.phase = SandboxPhase::Paused;
    Ok(())
}

pub(super) async fn resume<B: Backend>(state: &mut State<B>) -> Result<(), SandboxError> {
    require_capability(
        state.backend.capabilities().pause_resume,
        "pause and resume",
    )?;
    need(state, SandboxPhase::Paused, "resume")?;
    let id = required_id(state)?;
    if let Err(err) = state.backend.resume(&id).await {
        return cleanup_engine_error(state, err).await;
    }
    state.phase = SandboxPhase::Running;
    Ok(())
}

pub(super) async fn cleanup_engine_error<B: Backend, T>(
    state: &mut State<B>,
    err: EngineError,
) -> Result<T, SandboxError> {
    let report = destroy_report(state).await;
    if state.phase == SandboxPhase::CleanupRequired {
        return Err(SandboxError::CleanupRequired {
            instance_id: report.instance_id.unwrap_or_else(|| state.name.clone()),
            ownership: report.ownership,
            detail: format!("{}; {}", err.message, report.detail),
            failure: CleanupFailure::Backend {
                message: err.message,
            },
        });
    }
    Err(engine_err(err))
}

fn cleanup_required(
    instance_id: String,
    ownership: Ownership,
    detail: String,
    failure: CleanupFailure,
) -> SandboxError {
    SandboxError::CleanupRequired {
        instance_id,
        ownership,
        detail,
        failure,
    }
}

pub(super) async fn destroy_state<B: Backend>(state: &mut State<B>) -> Result<(), SandboxError> {
    if state.phase == SandboxPhase::Destroyed {
        return Ok(());
    }
    // Closing the transport never claims the guest or its descendants stopped,
    // so stop the attached process first and keep its completion sender in
    // `State` until this attempt has reported actual ownership.
    super::process::stop(state).await;
    let instance_id = state
        .id
        .as_ref()
        .map_or_else(|| state.name.clone(), |id| id.as_str().to_string());
    let result = destroy_resources(state).await;
    let metrics = state.metrics.clone();
    super::process::destroyed(state.process.take(), Some(instance_id), &result, &metrics);
    result
}

async fn destroy_resources<B: Backend>(state: &mut State<B>) -> Result<(), SandboxError> {
    state.phase = SandboxPhase::CleanupRequired;
    let retained = state.id.clone();
    let instance_id = retained
        .as_ref()
        .map_or_else(|| state.name.clone(), |id| id.as_str().to_string());
    let recovery = state
        .backend
        .recover_owned(&state.name, &state.nonce, retained.as_ref())
        .await;
    match recovery {
        Recovered::Removed => {}
        Recovered::Absent if state.create_unsettled => {
            return Err(cleanup_required(
                state.name.clone(),
                Ownership::Unknown,
                "create response is unresolved; owned instance may materialize later".to_string(),
                CleanupFailure::UnresolvedCreate {
                    instance_id: state.name.clone(),
                },
            ));
        }
        Recovered::Absent => {}
        Recovered::StillPresent { detail } => {
            return Err(cleanup_required(
                instance_id.clone(),
                Ownership::StillPresent,
                detail.clone(),
                CleanupFailure::StillPresent {
                    instance_id,
                    detail,
                },
            ));
        }
        Recovered::Unknown { detail } => {
            return Err(cleanup_required(
                instance_id,
                Ownership::Unknown,
                detail.clone(),
                CleanupFailure::Unknown { detail },
            ));
        }
    }
    state.create_unsettled = false;
    let workspace = Arc::clone(&state.workspace);
    // Do not clear the owned identity or discard staging on a failed cleanup.
    let staged = spawn_block(move || {
        workspace
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .cleanup()
    })
    .await;
    match staged {
        Ok(Ok(())) => {
            state.id = None;
            state.phase = SandboxPhase::Destroyed;
            Ok(())
        }
        Ok(Err(err)) => {
            let detail = format!("staging cleanup failed: {err}");
            Err(cleanup_required(
                state.name.clone(),
                Ownership::ConfirmedAbsent,
                detail,
                CleanupFailure::Staging(err),
            ))
        }
        Err(err) => {
            let detail = format!("staging cleanup task failed: {err}");
            Err(cleanup_required(
                state.name.clone(),
                Ownership::ConfirmedAbsent,
                detail,
                CleanupFailure::Backend {
                    message: err.to_string(),
                },
            ))
        }
    }
}

pub(super) async fn write_file<B: Backend>(
    state: &mut State<B>,
    path: &str,
    bytes: Vec<u8>,
) -> Result<(), SandboxError> {
    check_relative(path)?;
    if !matches!(state.phase, SandboxPhase::Created | SandboxPhase::Running) {
        return Err(phase_err(state, "write_file"));
    }
    let limit = state.config.limits().max_file_bytes();
    if bytes.len() > limit {
        return Err(WorkspaceError::LimitExceeded {
            limit,
            actual: bytes.len(),
        }
        .into());
    }
    if state.phase == SandboxPhase::Created {
        let workspace = Arc::clone(&state.workspace);
        let path = path.to_string();
        return Ok(spawn_block(move || {
            workspace
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .write_file(path, &bytes)
        })
        .await??);
    }
    let id = required_id(state)?;
    let result = state
        .backend
        .upload(
            &id,
            &state.user,
            state.config.isolation().workspace_path(),
            UploadEntry {
                path: path.to_string(),
                source: UploadSource::Bytes(bytes),
            },
            state.config.limits().workspace_bytes(),
        )
        .await;
    file_result(state, path, result).await
}

pub(super) async fn read_file<B: Backend>(
    state: &mut State<B>,
    path: &str,
) -> Result<Vec<u8>, SandboxError> {
    check_relative(path)?;
    if state.phase == SandboxPhase::Created {
        let workspace = Arc::clone(&state.workspace);
        let path = path.to_string();
        return Ok(spawn_block(move || {
            workspace
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .read_file(path)
        })
        .await??);
    }
    need(state, SandboxPhase::Running, "read_file")?;
    let id = required_id(state)?;
    let result = state
        .backend
        .download_file(
            &id,
            &state.user,
            state.config.isolation().workspace_path(),
            path,
            state.config.limits().max_file_bytes(),
        )
        .await;
    file_result(state, path, result).await
}

pub(super) async fn mkdir<B: Backend>(
    state: &mut State<B>,
    path: &str,
) -> Result<(), SandboxError> {
    check_relative(path)?;
    if state.phase == SandboxPhase::Created {
        let workspace = Arc::clone(&state.workspace);
        let path = path.to_string();
        return Ok(spawn_block(move || {
            workspace
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .create_dir_all(path)
        })
        .await??);
    }
    need(state, SandboxPhase::Running, "create_dir_all")?;
    let id = required_id(state)?;
    let result = state
        .backend
        .upload(
            &id,
            &state.user,
            state.config.isolation().workspace_path(),
            UploadEntry {
                path: path.to_string(),
                source: UploadSource::Directory,
            },
            state.config.limits().workspace_bytes(),
        )
        .await;
    file_result(state, path, result).await
}

pub(super) async fn unlink<B: Backend>(
    state: &mut State<B>,
    path: &str,
) -> Result<(), SandboxError> {
    check_relative(path)?;
    if state.phase == SandboxPhase::Created {
        let workspace = Arc::clone(&state.workspace);
        let path = path.to_string();
        return Ok(spawn_block(move || {
            workspace
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .remove_file(path)
        })
        .await??);
    }
    need(state, SandboxPhase::Running, "remove_file")?;
    let id = required_id(state)?;
    let result = state
        .backend
        .remove_file(
            &id,
            &state.user,
            state.config.isolation().workspace_path(),
            path,
        )
        .await;
    file_result(state, path, result).await
}

pub(super) async fn exec<B: Backend>(
    state: &mut State<B>,
    request: CommandRequest,
) -> Result<CommandOutput, SandboxError> {
    need(state, SandboxPhase::Running, "exec")?;
    if state.process.is_some() {
        return Err(SandboxError::ProcessActive);
    }
    if request.output_limit_bytes > state.config.output_limit_bytes() {
        return Err(SandboxError::OutputLimit {
            limit: state.config.output_limit_bytes(),
            phase: OutputPhase::NotStarted,
            ownership: Ownership::StillPresent,
            detail: "request exceeds configured output limit".to_string(),
        });
    }
    let id = required_id(state)?;
    let result = state
        .backend
        .exec(
            &id,
            &state.user,
            state.config.isolation().workspace_path(),
            &request.argv,
            request.output_limit_bytes,
        )
        .await;
    match result {
        Ok(done) => Ok(CommandOutput {
            exit_code: done.exit_code,
            stdout: done.stdout,
            stderr: done.stderr,
        }),
        Err(err) if err.kind == EngineErrorKind::Limit => {
            // stdout and stderr shared the request budget; report which stream
            // crossed it and reclaim the whole instance. Cleanup failure keeps
            // ownership reported rather than turning the error into success.
            let phase = match err.output.map(|output| output.phase) {
                Some(EngineOutputPhase::Stderr) => OutputPhase::Stderr,
                // An engine Limit without stream metadata is defensive only:
                // command output limits always originate in the collector.
                Some(EngineOutputPhase::Stdout) | None => OutputPhase::Stdout,
            };
            let report = destroy_report(state).await;
            Err(SandboxError::OutputLimit {
                limit: request.output_limit_bytes,
                phase,
                ownership: report.ownership,
                detail: report.detail,
            })
        }
        Err(err) => cleanup_engine_error(state, err).await,
    }
}

pub(super) async fn destroy_for_cancel<B: Backend, T>(
    state: &mut State<B>,
    operation: SandboxOperation,
    detail: &str,
) -> Result<T, SandboxError> {
    let report = destroy_report(state).await;
    Err(SandboxError::Cancelled {
        instance_id: report.instance_id,
        ownership: report.ownership,
        operation,
        detail: format!("{detail}; {}", report.detail),
    })
}

/// One cleanup attempt with both its observation and the caller-visible typed
/// failure, so explicit `destroy` can preserve the source chain.
pub(super) async fn destroy_attempt<B: Backend>(
    state: &mut State<B>,
) -> (CleanupReport, Option<SandboxError>) {
    let fallback_id = state.id.as_ref().map(|id| id.as_str().to_string());
    let error = destroy_state(state).await.err();
    let report = match &error {
        Some(SandboxError::CleanupRequired {
            instance_id,
            ownership,
            detail,
            ..
        }) => CleanupReport {
            instance_id: Some(instance_id.clone()),
            ownership: *ownership,
            detail: detail.clone(),
        },
        Some(err) => CleanupReport {
            instance_id: fallback_id.or_else(|| Some(state.name.clone())),
            ownership: Ownership::Unknown,
            detail: err.to_string(),
        },
        None => CleanupReport {
            instance_id: None,
            ownership: Ownership::ConfirmedAbsent,
            detail: "owned instance confirmed absent and staging removed".to_string(),
        },
    };
    state.cleanup.send_replace(super::cleanup::Progress {
        report: report.clone(),
        complete: state.phase == SandboxPhase::Destroyed,
        attempted: true,
    });
    state.metrics.record_cleanup_attempt();
    if error.is_some() {
        state.metrics.record_cleanup_failure();
    }
    events::cleanup_attempted(
        super::cleanup::ownership_label(report.ownership),
        state.phase == SandboxPhase::Destroyed,
        state.creation_cleanup,
    );
    (report, error)
}

pub(super) async fn destroy_report<B: Backend>(state: &mut State<B>) -> CleanupReport {
    destroy_attempt(state).await.0
}

pub(super) async fn status<B: Backend>(state: &State<B>) -> Result<SandboxStatus, SandboxError> {
    let Some(id) = state.id.as_ref() else {
        return Ok(SandboxStatus {
            phase: state.phase,
            instance_id: None,
            enforcement: None,
        });
    };
    let report = state.backend.host_report(id).await.map_err(engine_err)?;
    Ok(SandboxStatus {
        phase: state.phase,
        instance_id: Some(id.as_str().to_string()),
        enforcement: Some(EnforcementReport {
            runtime: report.runtime,
            memory_bytes: report.memory_bytes,
            nano_cpus: report.nano_cpus,
            pids_limit: report.pids_limit,
            readonly_root: report.readonly_root,
            network_mode: report.network_mode,
            cap_drop: report.cap_drop,
            security_opt: report.security_opt,
            bind_count: report.bind_count,
            tmpfs_workspace: report
                .tmpfs
                .as_ref()
                .and_then(|tmpfs| tmpfs.values().next().cloned()),
        }),
    })
}

pub(super) fn need<B: Backend>(
    state: &State<B>,
    phase: SandboxPhase,
    operation: &'static str,
) -> Result<(), SandboxError> {
    if state.phase == phase {
        Ok(())
    } else {
        Err(phase_err(state, operation))
    }
}

pub(super) fn phase_err<B: Backend>(state: &State<B>, operation: &'static str) -> SandboxError {
    SandboxError::InvalidPhase {
        current: state.phase,
        operation,
    }
}

pub(super) fn required_id<B: Backend>(state: &State<B>) -> Result<InstanceId, SandboxError> {
    state
        .id
        .clone()
        .ok_or_else(|| SandboxError::CleanupRequired {
            instance_id: state.name.clone(),
            ownership: Ownership::Unknown,
            detail: "instance id is not retained".to_string(),
            failure: CleanupFailure::Unknown {
                detail: "instance id is not retained".to_string(),
            },
        })
}

pub(super) fn path_text(path: &Path) -> Result<String, SandboxError> {
    path.to_str()
        .map(str::to_string)
        .ok_or(SandboxError::RelativePath {
            reason: "path is not utf-8",
        })
}

pub(super) async fn file_result<B: Backend, T>(
    state: &mut State<B>,
    path: &str,
    result: Result<T, EngineError>,
) -> Result<T, SandboxError> {
    match result {
        Ok(value) => Ok(value),
        Err(err) => match err.kind {
            EngineErrorKind::UnsafeFile => {
                Err(WorkspaceError::NotRegularFile { path: path.into() }.into())
            }
            EngineErrorKind::Limit => match err.output.map(|output| output.phase) {
                // stdout exceeding `max_bytes + 1` proves the guest file itself
                // is over the configured per-file bound.
                Some(EngineOutputPhase::Stdout) | None => Err(WorkspaceError::LimitExceeded {
                    limit: state.config.limits().max_file_bytes(),
                    actual: state.config.limits().max_file_bytes().saturating_add(1),
                }
                .into()),
                // The helper's separate 4 KiB stderr allowance is a transport
                // failure, not a file-size result: reclaim the instance.
                Some(EngineOutputPhase::Stderr) => cleanup_engine_error(state, err).await,
            },
            EngineErrorKind::NotFound => Err(WorkspaceError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                err.message,
            ))
            .into()),
            EngineErrorKind::PermissionDenied => Err(WorkspaceError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                err.message,
            ))
            .into()),
            EngineErrorKind::FileIo => {
                Err(WorkspaceError::Io(std::io::Error::other(err.message)).into())
            }
            EngineErrorKind::InvalidPath => Err(SandboxError::RelativePath {
                reason: "guest transport rejected the relative path",
            }),
            EngineErrorKind::Other | EngineErrorKind::Protocol | EngineErrorKind::EntryLimit => {
                cleanup_engine_error(state, err).await
            }
        },
    }
}

pub(super) fn engine_err(err: EngineError) -> SandboxError {
    if let Some(mismatch) = err.policy {
        return SandboxError::PolicyMismatch {
            field: mismatch.field.to_string(),
            expected: mismatch.expected,
            actual: mismatch.actual,
        };
    }
    // Output budget violations are mapped by `exec` (which knows the request
    // budget and observed ownership) and by `file_result` (where stdout means a
    // file-size result). Reaching this generic mapper with one is defensive;
    // its message still names the stream and limit.
    match err.kind {
        EngineErrorKind::InvalidPath => SandboxError::RelativePath {
            reason: "guest transport rejected the relative path",
        },
        EngineErrorKind::UnsafeFile
        | EngineErrorKind::Limit
        | EngineErrorKind::FileIo
        | EngineErrorKind::NotFound
        | EngineErrorKind::PermissionDenied
        | EngineErrorKind::Protocol
        | EngineErrorKind::EntryLimit
        | EngineErrorKind::Other => SandboxError::Backend {
            message: err.message,
        },
    }
}

pub(super) async fn spawn_block<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, SandboxError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| SandboxError::WorkerGone {
            detail: err.to_string(),
        })
}

/// Stable, non-sensitive error classification for telemetry.
///
/// Messages are deliberately not returned: they can contain host paths, guest
/// paths, or backend-supplied text.
pub(super) fn error_kind(err: &SandboxError) -> &'static str {
    match err {
        SandboxError::InvalidProcessRequest { .. } => "invalid_process_request",
        SandboxError::ProcessActive => "process_active",
        SandboxError::Config(_) => "config",
        SandboxError::BackendConfig(_) => "backend_config",
        SandboxError::Workspace(_) => "workspace",
        SandboxError::Snapshot(_) => "snapshot",
        SandboxError::SnapshotInterrupted { .. } => "snapshot_interrupted",
        SandboxError::Command(_) => "command",
        SandboxError::Staging(_) => "staging",
        SandboxError::InvalidPhase { .. } => "invalid_phase",
        SandboxError::BackendImageUnavailable { .. } => "backend_image_unavailable",
        SandboxError::Backend { .. } => "backend",
        SandboxError::TimedOut { .. } => "timed_out",
        SandboxError::Cancelled { .. } => "cancelled",
        SandboxError::OutputLimit { .. } => "output_limit",
        SandboxError::PolicyMismatch { .. } => "policy_mismatch",
        SandboxError::CreationCleanupRequired { .. } => "creation_cleanup_required",
        SandboxError::CleanupRequired { .. } => "cleanup_required",
        SandboxError::WorkerGone { .. } => "worker_gone",
        SandboxError::RelativePath { .. } => "relative_path",
        SandboxError::UnsupportedCapability { .. } => "unsupported_capability",
    }
}
