//! Environment-facing execution hook for [`EnvironmentPlan`].
//!
//! The `environment` domain deliberately has no lifecycle dependency. This
//! module owns the permitted `lifecycle -> environment` edge and performs the
//! real governed container work: resolve the explicit local image, create and
//! start one sandbox, execute every step with the public exec API, then destroy
//! it and wait for confirmed instance and staging cleanup on success or
//! failure. Dropping the returned future relies on the lifecycle owner's Drop
//! revocation, which continues cleanup independently of the dropped caller.

use std::time::Duration;

use crate::environment::{
    EnvironmentPlan, PreparationError, PrepareStep, PreparedEnvironment, StepObservation,
    prepare_docker_image,
};
use crate::runtime::{BackendConfig, SandboxConfig};

use super::{CommandRequest, Ownership, Sandbox, SandboxError};

const CLEANUP_CONFIRM: Duration = Duration::from_secs(60);

impl EnvironmentPlan {
    /// Runs the plan against one owned sandbox and confirms cleanup.
    ///
    /// The image is resolved using the request's pull policy (default local-only), then one sandbox is created,
    /// started, and used for each step in order. Each exec receives only the
    /// step's argv, timeout, and output bound; no host environment variable or
    /// credential is forwarded and no shell is involved. The sandbox is always
    /// destroyed, and this future returns only after the lifecycle receipt
    /// confirms both container absence and staging removal.
    ///
    /// A non-zero exit stops the plan at that step and returns
    /// [`PreparationError::StepFailed`] with the step index, exact argv, exit
    /// code, and bounded streams. A timeout or transport failure returns
    /// [`PreparationError::StepTimedOut`] or
    /// [`PreparationError::StepOperation`]. If cleanup cannot be confirmed, the
    /// step error carries a cleanup detail; a successful plan never hides an
    /// unconfirmed cleanup.
    ///
    /// Cancelling or dropping this future revokes the sandbox through its
    /// lifecycle owner. Cleanup continues in the detached worker, but a dropped
    /// future cannot report its result.
    ///
    /// # Errors
    ///
    /// Returns [`PreparationError`] before allocation for an output bound above
    /// the sandbox I/O limit or an unpinned step, for image resolution failure,
    /// for sandbox creation/start failure, for a failing step, or for
    /// unconfirmed cleanup.
    pub async fn execute(
        self,
        config: SandboxConfig,
    ) -> Result<PreparedEnvironment, PreparationError> {
        for (index, step) in self.steps().iter().enumerate() {
            if !step.is_pinned() {
                return Err(PreparationError::UnpinnedInput { index });
            }
            let sandbox_limit = config.output_limit_bytes();
            if step.output_limit_bytes() > sandbox_limit {
                return Err(PreparationError::OutputLimitExceedsSandbox {
                    index,
                    limit: step.output_limit_bytes(),
                    sandbox_limit,
                });
            }
        }

        let image = prepare_docker_image(self.image().clone()).await?;
        let backend = BackendConfig::docker(self.image().endpoint(), image.id(), "runc");
        let sandbox = create_sandbox(backend, config).await?;

        let outcome = match sandbox.start().await {
            Ok(()) => run_steps(&sandbox, self.steps()).await,
            Err(error) => Err(StepFailure::setup(error)),
        };
        let cleanup = release(&sandbox).await;

        match (outcome, cleanup) {
            (Ok(observations), Ok(())) => Ok(PreparedEnvironment::new(
                image,
                self.image().platform().clone(),
                self.steps().to_vec(),
                observations,
            )),
            (Ok(_), Err(detail)) => Err(PreparationError::Cleanup { detail }),
            (Err(failure), cleanup) => Err(failure.into_error(cleanup.err())),
        }
    }
}

async fn create_sandbox(
    backend: BackendConfig,
    config: SandboxConfig,
) -> Result<Sandbox, PreparationError> {
    match Sandbox::create(backend, config).await {
        Ok(sandbox) => Ok(sandbox),
        Err(SandboxError::CreationCleanupRequired {
            source, cleanup, ..
        }) => match tokio::time::timeout(CLEANUP_CONFIRM, cleanup.wait()).await {
            Ok(Ok(_)) => Err(PreparationError::Sandbox {
                detail: source.to_string(),
            }),
            Ok(Err(error)) => Err(PreparationError::Cleanup {
                detail: format!("creation failed: {source}; cleanup: {error}"),
            }),
            Err(_) => Err(PreparationError::Cleanup {
                detail: format!(
                    "creation failed: {source}; cleanup not confirmed within {CLEANUP_CONFIRM:?}"
                ),
            }),
        },
        Err(error) => Err(PreparationError::Sandbox {
            detail: error.to_string(),
        }),
    }
}

enum StepFailure {
    Exit {
        index: usize,
        argv: Vec<String>,
        exit_code: i64,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        output_limit_bytes: usize,
    },
    TimedOut {
        index: usize,
        argv: Vec<String>,
        timeout: Duration,
    },
    Operation {
        index: Option<usize>,
        argv: Vec<String>,
        detail: String,
    },
}

impl StepFailure {
    fn setup(error: SandboxError) -> Self {
        Self::Operation {
            index: None,
            argv: Vec::new(),
            detail: error.to_string(),
        }
    }

    fn into_error(self, cleanup: Option<String>) -> PreparationError {
        match self {
            Self::Exit {
                index,
                argv,
                exit_code,
                stdout,
                stderr,
                output_limit_bytes,
            } => PreparationError::StepFailed {
                index,
                argv,
                exit_code,
                stdout,
                stderr,
                output_limit_bytes,
                cleanup,
            },
            Self::TimedOut {
                index,
                argv,
                timeout,
            } => PreparationError::StepTimedOut {
                index,
                argv,
                timeout,
                cleanup,
            },
            Self::Operation {
                index: Some(index),
                argv,
                detail,
            } => PreparationError::StepOperation {
                index,
                argv,
                detail,
                cleanup,
            },
            Self::Operation {
                index: None,
                detail,
                ..
            } => match cleanup {
                Some(cleanup) => PreparationError::Cleanup {
                    detail: format!("{detail}; cleanup: {cleanup}"),
                },
                None => PreparationError::Sandbox { detail },
            },
        }
    }
}

async fn run_steps(
    sandbox: &Sandbox,
    steps: &[PrepareStep],
) -> Result<Vec<StepObservation>, StepFailure> {
    let mut observations = Vec::with_capacity(steps.len());
    for (index, step) in steps.iter().enumerate() {
        let request = CommandRequest {
            argv: step.argv().to_vec(),
            timeout: step.timeout(),
            output_limit_bytes: step.output_limit_bytes(),
        };
        match sandbox.exec(request).await {
            Ok(output) if output.exit_code == 0 => {
                let Some(version) = step.version().cloned() else {
                    return Err(StepFailure::Operation {
                        index: Some(index),
                        argv: step.argv().to_vec(),
                        detail: "step has no pinned version".to_string(),
                    });
                };
                observations.push(StepObservation::new(
                    index,
                    step.argv().to_vec(),
                    version,
                    output.exit_code,
                    output.stdout,
                    output.stderr,
                    step.output_limit_bytes(),
                ));
            }
            Ok(output) => {
                return Err(StepFailure::Exit {
                    index,
                    argv: step.argv().to_vec(),
                    exit_code: output.exit_code,
                    stdout: output.stdout,
                    stderr: output.stderr,
                    output_limit_bytes: step.output_limit_bytes(),
                });
            }
            Err(SandboxError::TimedOut { .. }) => {
                return Err(StepFailure::TimedOut {
                    index,
                    argv: step.argv().to_vec(),
                    timeout: step.timeout(),
                });
            }
            Err(error) => {
                return Err(StepFailure::Operation {
                    index: Some(index),
                    argv: step.argv().to_vec(),
                    detail: error.to_string(),
                });
            }
        }
    }
    Ok(observations)
}

async fn release(sandbox: &Sandbox) -> Result<(), String> {
    let destroy = sandbox.destroy().await;
    let receipt = sandbox.revoke();
    match tokio::time::timeout(CLEANUP_CONFIRM, receipt.wait()).await {
        Ok(Ok(report)) if report.ownership == Ownership::ConfirmedAbsent => Ok(()),
        Ok(Ok(report)) => {
            let ownership = report.ownership;
            let detail = report.detail;
            Err(format!("cleanup reported {ownership:?}: {detail}"))
        }
        Ok(Err(error)) => Err(match destroy {
            Ok(()) => error.to_string(),
            Err(destroy) => format!("destroy failed: {destroy}; receipt: {error}"),
        }),
        Err(_) => Err(format!("cleanup not confirmed within {CLEANUP_CONFIRM:?}")),
    }
}
