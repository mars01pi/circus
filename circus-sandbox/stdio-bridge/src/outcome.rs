use circus_agent_runner::{ModelFailure, TaskError, TaskOutcome};

use crate::protocol::{Cleanup, Failure, FinalResult, artifacts};

/// `include_agent_session` is set only when the caller opted into durable
/// multi-turn; otherwise the v1 success shape remains byte-compatible.
pub fn present(outcome: &TaskOutcome, include_agent_session: bool) -> (FinalResult, Cleanup) {
    match outcome {
        Ok(output) => (
            FinalResult::Succeeded {
                cleanup: Cleanup::Confirmed,
                artifacts: artifacts(&output.artifacts),
                agent_session: include_agent_session
                    .then(|| output.agent_session.clone())
                    .flatten(),
            },
            Cleanup::Confirmed,
        ),
        Err(failure) => {
            let cleanup = if failure.cleanup.is_some() {
                Cleanup::Pending
            } else if untracked_cleanup(&failure.error) {
                Cleanup::Unknown
            } else {
                Cleanup::Confirmed
            };
            let code = code(&failure.error);
            let message = if code == "cancelled" {
                "Agent task cancelled"
            } else {
                "Agent task failed; inspect the error code and cleanup status"
            };
            let error = Failure { code, message };
            let result = if code == "cancelled" {
                FinalResult::Cancelled { cleanup, error }
            } else {
                FinalResult::Failed { cleanup, error }
            };
            (result, cleanup)
        }
    }
}

pub fn after_cleanup_retry(outcome: &TaskOutcome) -> Cleanup {
    if outcome
        .as_ref()
        .err()
        .is_some_and(|failure| untracked_cleanup(&failure.error))
    {
        Cleanup::Unknown
    } else {
        Cleanup::Confirmed
    }
}

fn untracked_cleanup(error: &TaskError) -> bool {
    match error {
        TaskError::WorkerGone | TaskError::HostCleanup(_) => true,
        TaskError::Cleanup {
            task: Some(task), ..
        }
        | TaskError::Workspace {
            task: Some(task), ..
        } => untracked_cleanup(task),
        _ => false,
    }
}

fn code(error: &TaskError) -> &'static str {
    match error {
        TaskError::Invalid(_) | TaskError::NoRuntime => "invalid_request",
        TaskError::Cancelled => "cancelled",
        TaskError::TimedOut => "timed_out",
        TaskError::EventBackpressure => "backpressure",
        TaskError::Sandbox { .. } => "sandbox",
        TaskError::HostIo(_) => "host_io",
        TaskError::Workspace { .. } => "workspace",
        TaskError::AgentFailed => "agent_failed",
        TaskError::UnsupportedInteraction => "unsupported_interaction",
        TaskError::Protocol => "protocol",
        TaskError::Process => "process",
        TaskError::ArtifactLimit => "artifact_limit",
        TaskError::HostCleanup(_) | TaskError::Cleanup { .. } => "cleanup",
        TaskError::WorkerGone => "worker_gone",
        TaskError::Model(model) => match model {
            ModelFailure::Tls => "model_tls",
            ModelFailure::Timeout => "model_timeout",
            ModelFailure::Rejected { .. } => "model_rejected",
            ModelFailure::Transport => "model_transport",
            ModelFailure::Protocol => "model_protocol",
            ModelFailure::Budget => "model_budget",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use circus_agent_runner::{TaskFailure, TaskOutput};
    use std::collections::BTreeMap;

    #[test]
    fn success_exposes_agent_session_only_for_durable_callers() {
        let outcome: TaskOutcome = Ok(TaskOutput {
            task_id: "task-1".into(),
            artifacts: BTreeMap::new(),
            agent_session: Some("thread-1".into()),
        });
        let (durable, _) = present(&outcome, true);
        let serialized = sonic_rs::to_string(&durable).unwrap();
        assert!(serialized.contains(r#""agentSession":"thread-1""#));
        let (legacy, _) = present(&outcome, false);
        assert!(
            !sonic_rs::to_string(&legacy)
                .unwrap()
                .contains("agentSession")
        );
    }

    #[test]
    fn worker_loss_and_host_cleanup_are_never_confirmed_by_sandbox_retry() {
        for error in [
            TaskError::WorkerGone,
            TaskError::HostCleanup(std::io::Error::other("private host details")),
            TaskError::Cleanup {
                task: Some(Box::new(TaskError::HostCleanup(std::io::Error::other(
                    "private host details",
                )))),
                source: Box::new(circus_sandbox::SandboxError::ProcessActive),
            },
        ] {
            let outcome = Err(TaskFailure {
                error,
                cleanup: None,
            });
            assert!(after_cleanup_retry(&outcome) == Cleanup::Unknown);
            let (result, cleanup) = present(&outcome, true);
            assert!(cleanup == Cleanup::Unknown);
            assert!(
                !sonic_rs::to_string(&result)
                    .unwrap()
                    .contains("private host details")
            );
        }
    }
}
