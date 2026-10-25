//! One proxy run: recovered state, optional archive, sandbox, raw stdio, and
//! confirmed finalization.

use std::sync::Arc;

use circus_sandbox::{
    CleanupReport, Ownership, ProcessExit, ProcessTermination, RecoveryId, Sandbox, SandboxError,
    SandboxPhase,
};

use crate::archive::{self, ArchiveError};
use crate::backend::BackendError;
use crate::config::{Config, ConfigLoadError};
use crate::proxy::{Proxy, ProxyEnd, ShutdownReason, Signals};
use crate::state::{self, State, StateError, StatePhase};

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("invalid configuration: {0}")]
    Config(#[from] ConfigLoadError),
    #[error("recovery state: {0}")]
    State(#[from] StateError),
    #[error("workspace archive: {0}")]
    Archive(#[from] ArchiveError),
    #[error("backend: {0}")]
    Backend(#[from] BackendError),
    #[error("host signal registration failed")]
    Signals,
    #[error(
        "a previous sandbox may still exist; recovery did not confirm removal, so this run refuses to create a duplicate"
    )]
    OrphanUnconfirmed,
    #[error("orphan recovery failed ({kind})")]
    Recovery { kind: &'static str },
    #[error("sandbox creation failed ({kind})")]
    Create { kind: &'static str },
    #[error("sandbox {stage} failed ({kind})")]
    Startup {
        stage: &'static str,
        kind: &'static str,
    },
    #[error("the process session ended without an outcome")]
    SessionLost,
    #[error(
        "workspace capture or archive write failed; sandbox cleanup confirmed; the previous archive remains"
    )]
    Capture,
    #[error("sandbox cleanup could not be confirmed; state is retained for recovery")]
    Cleanup,
    #[error("cleanup was confirmed but the state file could not be updated")]
    StateWrite,
    #[error("process {reason}; cleanup confirmed; workspace archive {archive}")]
    Terminated {
        reason: &'static str,
        archive: &'static str,
    },
    #[error("interrupted by {signal}; cleanup confirmed; workspace archive {archive}")]
    Interrupted {
        signal: &'static str,
        archive: &'static str,
    },
}

impl RunError {
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Config(_) => 2,
            Self::Interrupted {
                signal: "SIGINT" | "interrupt",
                ..
            } => 130,
            Self::Interrupted {
                signal: "SIGTERM", ..
            } => 143,
            _ => 1,
        }
    }
}

/// Full absence and completed staging cleanup are both required before state is
/// closed. A confirmed-absent instance whose staging cleanup failed still needs
/// another destroy attempt.
pub(crate) fn cleanup_confirmed(report: &CleanupReport) -> bool {
    report.ownership == Ownership::ConfirmedAbsent && report.instance_id.is_none()
}

fn guest_exit_code(code: i64) -> u8 {
    u8::try_from(code).unwrap_or(1)
}

fn termination_text(reason: &ProcessTermination) -> &'static str {
    match reason {
        ProcessTermination::Cancelled => "cancelled",
        ProcessTermination::TimedOut => "timed out",
        ProcessTermination::OutputLimit { .. } => "exceeded its output limit",
        ProcessTermination::Transport { .. } => "lost its transport",
        ProcessTermination::WorkerFailed { .. } => "lost its lifecycle worker",
    }
}

/// Maps sandbox failures to static categories so diagnostics never echo argv,
/// environment values, or backend-provided detail.
fn sandbox_error_kind(error: &SandboxError) -> &'static str {
    match error {
        SandboxError::InvalidProcessRequest { .. } => "invalid process request",
        SandboxError::ProcessActive => "process already active",
        SandboxError::Config(_) => "configuration",
        SandboxError::BackendConfig(_) => "backend configuration",
        SandboxError::Workspace(_) => "workspace",
        SandboxError::Snapshot(_) | SandboxError::SnapshotInterrupted { .. } => {
            "workspace snapshot"
        }
        SandboxError::Command(_) => "command",
        SandboxError::Staging(_) => "staging",
        SandboxError::InvalidPhase { .. } => "invalid phase",
        SandboxError::BackendImageUnavailable { .. } => "image unavailable",
        SandboxError::Backend { .. } => "backend",
        SandboxError::TimedOut { .. } => "timeout",
        SandboxError::Cancelled { .. } => "cancelled",
        SandboxError::OutputLimit { .. } => "output limit",
        SandboxError::PolicyMismatch { .. } => "policy mismatch",
        SandboxError::CreationCleanupRequired { .. } => "creation cleanup",
        SandboxError::CleanupRequired { .. } => "cleanup",
        SandboxError::WorkerGone { .. } => "worker gone",
        SandboxError::RelativePath { .. } => "relative path",
        SandboxError::UnsupportedCapability { .. } => "unsupported capability",
    }
}

fn state_for(config: &Config, recovery: &RecoveryId, phase: StatePhase) -> State {
    State::new(recovery.clone(), config.backend.kind(), phase)
}

fn record_closed(config: &Config, recovery: &RecoveryId) -> Result<(), RunError> {
    state::write(
        &config.state_path,
        &state_for(config, recovery, StatePhase::Closed),
    )
    .map_err(|_| RunError::StateWrite)
}

async fn save_workspace(sandbox: &Sandbox, config: &Config) -> Result<(), ()> {
    let snapshot = sandbox
        .capture_workspace_snapshot(config.budget, config.archive_max_bytes)
        .await
        .map_err(|_| ())?;
    archive::save(snapshot, config.archive_path.clone())
        .await
        .map_err(|_| ())
}

/// A failed create can still owe backend cleanup. Wait for its owned receipt
/// within the lifecycle deadline; only a confirmed report closes state.
async fn note_create_failure(config: &Config, recovery: &RecoveryId, error: &SandboxError) {
    let SandboxError::CreationCleanupRequired { cleanup, .. } = error else {
        return;
    };
    let deadline = config.sandbox.lifecycle_timeout();
    if let Ok(Ok(report)) = tokio::time::timeout(deadline, cleanup.wait()).await
        && cleanup_confirmed(&report)
    {
        let _ = record_closed(config, recovery);
    }
}

async fn startup_failure(
    config: &Config,
    recovery: &RecoveryId,
    sandbox: &Sandbox,
    stage: &'static str,
    error: &SandboxError,
) -> RunError {
    let kind = sandbox_error_kind(error);
    if sandbox.destroy().await.is_err() {
        return RunError::Cleanup;
    }
    if record_closed(config, recovery).is_err() {
        return RunError::StateWrite;
    }
    RunError::Startup { stage, kind }
}

/// Runs one proxy session. The returned byte is the host process exit code.
///
/// Ordering is fixed: validate configuration and argv, recover any persisted
/// orphan, write conservative state, prepare the image, load the archive, then
/// create, restore, start, proxy, capture, destroy, and close state.
pub async fn execute(config: Config, argv: Vec<String>) -> Result<u8, RunError> {
    config.validate_argv(&argv)?;
    config.check_destinations()?;
    config.backend.ensure_available()?;
    let mut signals = Signals::new().map_err(|_| RunError::Signals)?;

    let (recovery, had_state) = match state::read(&config.state_path)? {
        Some(existing) => {
            state::validate_backend(&existing, config.backend.kind())?;
            let ownership =
                Sandbox::recover_orphan(config.backend.descriptor(), &existing.recovery_id)
                    .await
                    .map_err(|error| RunError::Recovery {
                        kind: sandbox_error_kind(&error),
                    })?;
            if ownership != Ownership::ConfirmedAbsent {
                return Err(RunError::OrphanUnconfirmed);
            }
            (existing.recovery_id, true)
        }
        None => {
            let recovery = RecoveryId::generate();
            state::write(
                &config.state_path,
                &state_for(&config, &recovery, StatePhase::Creating),
            )?;
            (recovery, false)
        }
    };
    if had_state {
        state::write(
            &config.state_path,
            &state_for(&config, &recovery, StatePhase::Creating),
        )?;
    }

    let backend = config.backend.prepare(&config.image).await?;
    let snapshot = archive::load(
        &config.archive_path,
        config.sandbox.limits().max_file_bytes(),
        config.budget,
    )
    .await?;

    let sandbox =
        match Sandbox::create_with_recovery_id(backend, config.sandbox.clone(), recovery.clone())
            .await
        {
            Ok(sandbox) => sandbox,
            Err(error) => {
                note_create_failure(&config, &recovery, &error).await;
                return Err(RunError::Create {
                    kind: sandbox_error_kind(&error),
                });
            }
        };

    if let Some(snapshot) = snapshot {
        let restored = sandbox.restore_workspace_snapshot(snapshot).await;
        if let Err(error) = restored {
            return Err(startup_failure(&config, &recovery, &sandbox, "restore", &error).await);
        }
    }
    if let Err(error) = sandbox.start().await {
        return Err(startup_failure(&config, &recovery, &sandbox, "start", &error).await);
    }
    if state::write(
        &config.state_path,
        &state_for(&config, &recovery, StatePhase::Running),
    )
    .is_err()
    {
        let confirmed = sandbox.destroy().await.is_ok();
        if confirmed {
            let _ = record_closed(&config, &recovery);
        }
        return Err(if confirmed {
            RunError::StateWrite
        } else {
            RunError::Cleanup
        });
    }

    let session = match sandbox.spawn_process(config.process.request(argv)).await {
        Ok(session) => Arc::new(session),
        Err(error) => {
            return Err(
                startup_failure(&config, &recovery, &sandbox, "process spawn", &error).await,
            );
        }
    };
    let mut proxy = Proxy::start(Arc::clone(&session));
    let end = proxy.next_end(&mut signals).await;

    match end {
        ProxyEnd::Exited { exit_code } => {
            proxy.stop_input().await;
            let saved = save_workspace(&sandbox, &config).await;
            proxy.drain_output().await;
            if sandbox.destroy().await.is_err() {
                proxy.finish().await;
                return Err(RunError::Cleanup);
            }
            proxy.finish().await;
            record_closed(&config, &recovery)?;
            if saved.is_err() {
                return Err(RunError::Capture);
            }
            Ok(guest_exit_code(exit_code))
        }
        ProxyEnd::Terminated { reason, cleanup } => {
            proxy.stop_input().await;
            let running = sandbox
                .status()
                .await
                .is_ok_and(|status| status.phase == SandboxPhase::Running);
            let saved = if running {
                save_workspace(&sandbox, &config).await
            } else {
                Err(())
            };
            let mut confirmed = cleanup_confirmed(&cleanup);
            if !confirmed {
                confirmed = sandbox.destroy().await.is_ok();
            }
            proxy.finish().await;
            if !confirmed {
                return Err(RunError::Cleanup);
            }
            record_closed(&config, &recovery)?;
            Err(RunError::Terminated {
                reason: termination_text(&reason),
                archive: archive_status(saved.is_ok()),
            })
        }
        ProxyEnd::Shutdown(reason) => {
            proxy.stop_input().await;
            let running = sandbox
                .status()
                .await
                .is_ok_and(|status| status.phase == SandboxPhase::Running);
            let saved = if running {
                save_workspace(&sandbox, &config).await
            } else {
                Err(())
            };
            session.cancel();
            let waited =
                tokio::time::timeout(config.sandbox.lifecycle_timeout(), session.wait()).await;
            let mut confirmed = match waited {
                Ok(Ok(ProcessExit::Terminated { cleanup, .. })) => cleanup_confirmed(&cleanup),
                _ => false,
            };
            if !confirmed {
                confirmed = sandbox.destroy().await.is_ok();
            }
            proxy.finish().await;
            if !confirmed {
                return Err(RunError::Cleanup);
            }
            record_closed(&config, &recovery)?;
            if reason == ShutdownReason::StdinEof {
                return if saved.is_ok() {
                    Ok(0)
                } else {
                    Err(RunError::Capture)
                };
            }
            Err(RunError::Interrupted {
                signal: reason.text(),
                archive: archive_status(saved.is_ok()),
            })
        }
        ProxyEnd::Lost => {
            proxy.stop_input().await;
            let running = sandbox
                .status()
                .await
                .is_ok_and(|status| status.phase == SandboxPhase::Running);
            let _ = if running {
                save_workspace(&sandbox, &config).await
            } else {
                Err(())
            };
            let confirmed = sandbox.destroy().await.is_ok();
            proxy.finish().await;
            if !confirmed {
                return Err(RunError::Cleanup);
            }
            record_closed(&config, &recovery)?;
            Err(RunError::SessionLost)
        }
    }
}

fn archive_status(saved: bool) -> &'static str {
    if saved { "saved" } else { "unchanged" }
}

/// Exposed for unit tests without requiring a backend.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_exit_codes_fit_the_host_process_model() {
        assert_eq!(guest_exit_code(0), 0);
        assert_eq!(guest_exit_code(1), 1);
        assert_eq!(guest_exit_code(255), 255);
        assert_eq!(guest_exit_code(256), 1);
        assert_eq!(guest_exit_code(-1), 1);
    }

    #[test]
    fn error_exit_codes_distinguish_configuration_and_signals() {
        assert_eq!(RunError::Config(ConfigLoadError::Json).exit_code(), 2);
        assert_eq!(
            RunError::Interrupted {
                signal: "SIGINT",
                archive: "saved"
            }
            .exit_code(),
            130
        );
        assert_eq!(
            RunError::Interrupted {
                signal: "interrupt",
                archive: "saved"
            }
            .exit_code(),
            130
        );
        assert_eq!(
            RunError::Interrupted {
                signal: "SIGTERM",
                archive: "saved"
            }
            .exit_code(),
            143
        );
        assert_eq!(
            RunError::Interrupted {
                signal: "stdin EOF",
                archive: "saved"
            }
            .exit_code(),
            1
        );
        assert_eq!(RunError::Cleanup.exit_code(), 1);
    }

    #[test]
    fn termination_text_never_echoes_backend_detail() {
        assert_eq!(
            termination_text(&ProcessTermination::Transport {
                detail: "secret detail".to_string()
            }),
            "lost its transport"
        );
        assert_eq!(
            termination_text(&ProcessTermination::OutputLimit { limit: 10 }),
            "exceeded its output limit"
        );
    }

    #[test]
    fn sandbox_errors_reduce_to_static_categories() {
        assert_eq!(
            sandbox_error_kind(&SandboxError::InvalidPhase {
                current: SandboxPhase::Created,
                operation: "exec"
            }),
            "invalid phase"
        );
        assert_eq!(
            sandbox_error_kind(&SandboxError::Backend {
                message: "host detail".to_string()
            }),
            "backend"
        );
    }
}
