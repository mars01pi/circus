//! Typed, ordered, reproducible preparation plans.
//!
//! A plan names an explicit local image request and an ordered list of guest
//! commands. Every step is executed with a separate argv vector, a per-step
//! deadline, and a per-step output bound through the public sandbox lifecycle;
//! no shell is involved and no host environment, credential, or Docker
//! configuration is inherited.
//!
//! A plan refuses an unpinned step at construction. Reproducibility is claimed
//! only for an [`EnvironmentPlan`] whose steps all carry a [`PinnedVersion`];
//! the pinned version and exact argv are recorded with the resolved image ID and
//! repository digests so a later [`EnvironmentId`] can be derived from them.
//!
//! Execution itself lives in the private lifecycle domain (the permitted
//! `lifecycle -> environment` dependency edge): `EnvironmentPlan::execute`
//! resolves the local image, creates and starts one sandbox, runs the steps,
//! destroys it, and returns only after owned instance and staging cleanup are
//! confirmed.

use std::fmt;
use std::time::Duration;

use super::identity::{EnvironmentId, IdentityError, TenantDomain};
use super::{EnvironmentError, ImagePlatform, ImageRequest, PreparedImage};

/// Exact version pin for one preparation step.
///
/// The string is caller-supplied metadata; this crate does not inspect the
/// guest to verify it. It must be nonempty, free of surrounding whitespace and
/// control characters, so that equivalent pins have one canonical spelling.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PinnedVersion(String);

impl PinnedVersion {
    /// Validates a pinned version string.
    ///
    /// # Errors
    ///
    /// Returns [`PlanError::InvalidVersion`] for an empty, untrimmed, or
    /// control-containing value.
    pub fn new(value: impl Into<String>) -> Result<Self, PlanError> {
        let value = value.into();
        if value.is_empty()
            || value.trim() != value
            || value.chars().any(|character| character.is_control())
        {
            return Err(PlanError::InvalidVersion);
        }
        Ok(Self(value))
    }

    /// Exact pinned version text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PinnedVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One explicitly ordered guest command with its pin and execution bounds.
///
/// `argv[0]` is the executable; every later element is a separate argument.
/// The command is never interpolated by a shell. A step built with
/// [`Self::unpinned`] is representable but is rejected by
/// [`EnvironmentPlan::new`] and by identity construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareStep {
    argv: Vec<String>,
    version: Option<PinnedVersion>,
    timeout: Duration,
    output_limit_bytes: usize,
}

impl PrepareStep {
    /// Builds a pinned step.
    ///
    /// # Errors
    ///
    /// Returns [`PlanError`] for empty argv, an empty executable, a NUL byte in
    /// any argument, a zero timeout, or a zero output bound.
    pub fn new(
        argv: Vec<String>,
        version: PinnedVersion,
        timeout: Duration,
        output_limit_bytes: usize,
    ) -> Result<Self, PlanError> {
        Self::build(argv, Some(version), timeout, output_limit_bytes)
    }

    /// Builds an explicitly unpinned step.
    ///
    /// The value exists so that unpinned input can be represented and rejected
    /// by [`EnvironmentPlan::new`] with [`PlanError::UnpinnedInput`] instead of
    /// being silently accepted.
    ///
    /// # Errors
    ///
    /// Returns [`PlanError`] for invalid argv, timeout, or output bound.
    pub fn unpinned(
        argv: Vec<String>,
        timeout: Duration,
        output_limit_bytes: usize,
    ) -> Result<Self, PlanError> {
        Self::build(argv, None, timeout, output_limit_bytes)
    }

    fn build(
        argv: Vec<String>,
        version: Option<PinnedVersion>,
        timeout: Duration,
        output_limit_bytes: usize,
    ) -> Result<Self, PlanError> {
        if argv.is_empty() {
            return Err(PlanError::EmptyArgv);
        }
        if argv[0].is_empty() {
            return Err(PlanError::EmptyProgram);
        }
        if argv.iter().any(|argument| argument.contains('\0')) {
            return Err(PlanError::NulArgument);
        }
        if timeout.is_zero() {
            return Err(PlanError::InvalidTimeout { timeout });
        }
        if output_limit_bytes == 0 {
            return Err(PlanError::InvalidOutputLimit);
        }
        Ok(Self {
            argv,
            version,
            timeout,
            output_limit_bytes,
        })
    }

    /// Exact argv, including the executable at index zero.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// Pinned version, or `None` when this step is explicitly unpinned.
    #[must_use]
    pub fn version(&self) -> Option<&PinnedVersion> {
        self.version.as_ref()
    }

    /// Whether this step carries a version pin.
    #[must_use]
    pub fn is_pinned(&self) -> bool {
        self.version.is_some()
    }

    /// Per-step command deadline.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Per-step captured stdout and stderr byte bound.
    #[must_use]
    pub fn output_limit_bytes(&self) -> usize {
        self.output_limit_bytes
    }
}

/// An explicit, ordered preparation plan.
///
/// Construction validates the image request and every step without contacting
/// Docker. No step may be unpinned: the plan refuses to claim reproducibility
/// when a version is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentPlan {
    image: ImageRequest,
    steps: Vec<PrepareStep>,
}

impl EnvironmentPlan {
    /// Builds an ordered plan.
    ///
    /// # Errors
    ///
    /// Returns [`PlanError::NoSteps`] for an empty plan and
    /// [`PlanError::UnpinnedInput`] for the first step without a pinned version.
    /// Invalid steps cannot be represented because [`PrepareStep`] validates
    /// them at construction.
    pub fn new(image: ImageRequest, steps: Vec<PrepareStep>) -> Result<Self, PlanError> {
        if steps.is_empty() {
            return Err(PlanError::NoSteps);
        }
        for (index, step) in steps.iter().enumerate() {
            if !step.is_pinned() {
                return Err(PlanError::UnpinnedInput { index });
            }
        }
        Ok(Self { image, steps })
    }

    /// Explicit local image request.
    #[must_use]
    pub fn image(&self) -> &ImageRequest {
        &self.image
    }

    /// Ordered steps as supplied.
    #[must_use]
    pub fn steps(&self) -> &[PrepareStep] {
        &self.steps
    }

    /// Derives a stable identity for this plan once image contents are known.
    ///
    /// The requested platform must match the inspected image OS and
    /// architecture; a variant, if requested, is carried into the identity but
    /// cannot be verified from Docker image inspection.
    ///
    /// # Errors
    ///
    /// Returns [`IdentityError`] for mismatched image metadata or invalid
    /// identity material.
    pub fn identity(
        &self,
        image: &PreparedImage,
        tenant: TenantDomain,
    ) -> Result<EnvironmentId, IdentityError> {
        verify_image_platform(image, self.image.platform())?;
        EnvironmentId::new(
            image.id().to_string(),
            image.repo_digests().to_vec(),
            self.image.platform().clone(),
            self.steps.clone(),
            tenant,
        )
    }
}

/// One successful step observation, including its bounded output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepObservation {
    index: usize,
    argv: Vec<String>,
    version: PinnedVersion,
    exit_code: i64,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    output_limit_bytes: usize,
}

impl StepObservation {
    pub(crate) fn new(
        index: usize,
        argv: Vec<String>,
        version: PinnedVersion,
        exit_code: i64,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        output_limit_bytes: usize,
    ) -> Self {
        Self {
            index,
            argv,
            version,
            exit_code,
            stdout,
            stderr,
            output_limit_bytes,
        }
    }

    /// Zero-based position in the plan.
    #[must_use]
    pub fn index(&self) -> usize {
        self.index
    }

    /// Exact argv that was executed.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// Pinned version recorded for this step.
    #[must_use]
    pub fn version(&self) -> &PinnedVersion {
        &self.version
    }

    /// Observed exit code. Successful observations are always zero.
    #[must_use]
    pub fn exit_code(&self) -> i64 {
        self.exit_code
    }

    /// Captured stdout, at most [`Self::output_limit_bytes`] bytes.
    #[must_use]
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    /// Captured stderr, at most [`Self::output_limit_bytes`] bytes.
    #[must_use]
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    /// Configured per-stream capture bound.
    #[must_use]
    pub fn output_limit_bytes(&self) -> usize {
        self.output_limit_bytes
    }
}

/// Resolved image and completed step observations for one prepared environment.
///
/// The value records the immutable image ID, repository digests, requested
/// platform, exact pinned steps, and bounded output. It owns no container,
/// cache entry, or storage handle: execution already destroyed the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedEnvironment {
    image: PreparedImage,
    platform: ImagePlatform,
    steps: Vec<PrepareStep>,
    observations: Vec<StepObservation>,
}

impl PreparedEnvironment {
    pub(crate) fn new(
        image: PreparedImage,
        platform: ImagePlatform,
        steps: Vec<PrepareStep>,
        observations: Vec<StepObservation>,
    ) -> Self {
        Self {
            image,
            platform,
            steps,
            observations,
        }
    }

    /// Immutable resolved image contents.
    #[must_use]
    pub fn image(&self) -> &PreparedImage {
        &self.image
    }

    /// Requested platform used for resolution and identity.
    #[must_use]
    pub fn platform(&self) -> &ImagePlatform {
        &self.platform
    }

    /// Exact pinned steps that were executed.
    #[must_use]
    pub fn steps(&self) -> &[PrepareStep] {
        &self.steps
    }

    /// Successful observations in execution order.
    #[must_use]
    pub fn observations(&self) -> &[StepObservation] {
        &self.observations
    }

    /// Derives this environment's stable identity for one tenant domain.
    ///
    /// # Errors
    ///
    /// Returns [`IdentityError`] if observed image metadata cannot support the
    /// recorded platform or any identity material is invalid.
    pub fn identity(&self, tenant: TenantDomain) -> Result<EnvironmentId, IdentityError> {
        verify_image_platform(&self.image, &self.platform)?;
        EnvironmentId::new(
            self.image.id().to_string(),
            self.image.repo_digests().to_vec(),
            self.platform.clone(),
            self.steps.clone(),
            tenant,
        )
    }
}

/// Why a preparation plan was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("a preparation plan requires at least one step")]
    NoSteps,
    #[error("preparation step {index} has no pinned version; reproducibility cannot be claimed")]
    UnpinnedInput { index: usize },
    #[error("preparation step argv must contain an executable")]
    EmptyArgv,
    #[error("preparation step executable must be nonempty")]
    EmptyProgram,
    #[error("preparation step argv contains a NUL byte")]
    NulArgument,
    #[error("preparation step timeout must be non-zero, got {timeout:?}")]
    InvalidTimeout { timeout: Duration },
    #[error("preparation step output limit must be non-zero")]
    InvalidOutputLimit,
    #[error("pinned version must be nonempty, trimmed, and free of control characters")]
    InvalidVersion,
}

/// Why environment preparation failed.
///
/// A failing step retains its zero-based index, exact argv, exit code, bounded
/// stdout/stderr bytes, and the per-stream `output_limit_bytes` bound. When
/// cleanup could not be confirmed, the optional `cleanup` detail records that;
/// otherwise it is `None` and owned instance and staging cleanup were confirmed
/// before this error was returned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PreparationError {
    #[error(transparent)]
    Image(#[from] EnvironmentError),
    #[error("preparation step {index} has no pinned version")]
    UnpinnedInput { index: usize },
    #[error(
        "preparation step {index} output limit {limit} exceeds the sandbox I/O limit {sandbox_limit}"
    )]
    OutputLimitExceedsSandbox {
        index: usize,
        limit: usize,
        sandbox_limit: usize,
    },
    #[error("sandbox preparation failed: {detail}")]
    Sandbox { detail: String },
    #[error("preparation step {index} (`{argv:?}`) exited with code {exit_code}")]
    StepFailed {
        index: usize,
        argv: Vec<String>,
        exit_code: i64,
        /// Captured stdout, at most `output_limit_bytes`.
        stdout: Vec<u8>,
        /// Captured stderr, at most `output_limit_bytes`.
        stderr: Vec<u8>,
        /// Per-stream capture bound configured for this step.
        output_limit_bytes: usize,
        cleanup: Option<String>,
    },
    #[error("preparation step {index} (`{argv:?}`) exceeded its {timeout:?} deadline")]
    StepTimedOut {
        index: usize,
        argv: Vec<String>,
        timeout: Duration,
        cleanup: Option<String>,
    },
    #[error("preparation step {index} (`{argv:?}`) failed: {detail}")]
    StepOperation {
        index: usize,
        argv: Vec<String>,
        detail: String,
        cleanup: Option<String>,
    },
    #[error("environment cleanup could not be confirmed: {detail}")]
    Cleanup { detail: String },
}

pub(super) fn verify_image_platform(
    image: &PreparedImage,
    platform: &ImagePlatform,
) -> Result<(), IdentityError> {
    if image.os() != platform.os() || image.architecture() != platform.architecture() {
        return Err(IdentityError::ImagePlatformMismatch {
            expected: platform.clone(),
            actual_os: image.os().to_string(),
            actual_architecture: image.architecture().to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform() -> ImagePlatform {
        ImagePlatform::new("linux", "amd64").expect("valid platform")
    }

    fn request() -> ImageRequest {
        ImageRequest::new(
            "unix:///var/run/docker.sock",
            "python:3.12-slim",
            platform(),
            Duration::from_secs(10),
        )
        .expect("valid request")
    }

    fn pinned(argv: &[&str], version: &str) -> PrepareStep {
        PrepareStep::new(
            argv.iter().map(|value| (*value).to_string()).collect(),
            PinnedVersion::new(version).expect("valid version"),
            Duration::from_secs(5),
            1024,
        )
        .expect("valid step")
    }

    #[test]
    fn plan_rejects_unpinned_and_empty_inputs() {
        let unpinned = PrepareStep::unpinned(
            vec!["python3".into(), "--version".into()],
            Duration::from_secs(1),
            1024,
        )
        .expect("valid argv");
        assert_eq!(
            EnvironmentPlan::new(request(), vec![unpinned]),
            Err(PlanError::UnpinnedInput { index: 0 })
        );
        assert_eq!(
            EnvironmentPlan::new(request(), Vec::new()),
            Err(PlanError::NoSteps)
        );
        assert_eq!(
            PrepareStep::new(
                Vec::new(),
                PinnedVersion::new("1").expect("valid version"),
                Duration::from_secs(1),
                1
            ),
            Err(PlanError::EmptyArgv)
        );
        assert_eq!(
            PrepareStep::new(
                vec![String::new()],
                PinnedVersion::new("1").expect("valid version"),
                Duration::from_secs(1),
                1
            ),
            Err(PlanError::EmptyProgram)
        );
        assert_eq!(
            PrepareStep::new(
                vec!["python3".into(), "bad\0argument".into()],
                PinnedVersion::new("1").expect("valid version"),
                Duration::from_secs(1),
                1
            ),
            Err(PlanError::NulArgument)
        );
        assert_eq!(
            PrepareStep::new(
                vec!["python3".into()],
                PinnedVersion::new("1").expect("valid version"),
                Duration::ZERO,
                1
            ),
            Err(PlanError::InvalidTimeout {
                timeout: Duration::ZERO
            })
        );
        assert_eq!(
            PrepareStep::new(
                vec!["python3".into()],
                PinnedVersion::new("1").expect("valid version"),
                Duration::from_secs(1),
                0
            ),
            Err(PlanError::InvalidOutputLimit)
        );
        assert_eq!(PinnedVersion::new(" 1"), Err(PlanError::InvalidVersion));
    }

    #[test]
    fn plan_and_step_accessors_expose_configured_bounds() {
        let step = pinned(&["python3", "--version"], "3.12.1");
        assert_eq!(
            step.argv(),
            &["python3".to_string(), "--version".to_string()]
        );
        assert_eq!(step.version().map(PinnedVersion::as_str), Some("3.12.1"));
        assert!(step.is_pinned());
        assert_eq!(step.timeout(), Duration::from_secs(5));
        assert_eq!(step.output_limit_bytes(), 1024);
        assert_eq!(PinnedVersion::new("3.12.1").unwrap().to_string(), "3.12.1");

        let plan = EnvironmentPlan::new(request(), vec![step.clone()]).expect("valid plan");
        assert_eq!(plan.image().reference(), request().reference());
        assert_eq!(plan.steps().len(), 1);
        assert_eq!(plan.steps()[0], step);

        let observation = StepObservation::new(
            0,
            vec!["python3".into(), "--version".into()],
            PinnedVersion::new("3.12.1").expect("version"),
            0,
            b"ok".to_vec(),
            Vec::new(),
            1024,
        );
        assert_eq!(observation.index(), 0);
        assert_eq!(
            observation.argv(),
            &["python3".to_string(), "--version".to_string()]
        );
        assert_eq!(observation.version().as_str(), "3.12.1");
        assert_eq!(observation.exit_code(), 0);
        assert_eq!(observation.stdout(), b"ok");
        assert!(observation.stderr().is_empty());
        assert_eq!(observation.output_limit_bytes(), 1024);

        let prepared = PreparedEnvironment::new(
            PreparedImage {
                id: "sha256:1111".into(),
                repo_digests: vec!["python@sha256:2222".into()],
                os: "linux".into(),
                architecture: "amd64".into(),
            },
            platform(),
            vec![step],
            vec![observation],
        );
        assert_eq!(prepared.image().id(), "sha256:1111");
        assert_eq!(prepared.platform(), &platform());
        assert_eq!(prepared.steps().len(), 1);
        assert_eq!(prepared.observations().len(), 1);
        let identity = prepared
            .identity(TenantDomain::new("tenant-a").expect("valid tenant"))
            .expect("identity");
        assert!(!format!("{identity:?}").contains("python"));
    }

    #[test]
    fn plan_identity_tracks_steps_and_tenant_without_host_paths() {
        let plan =
            EnvironmentPlan::new(request(), vec![pinned(&["python3", "--version"], "3.12.1")])
                .expect("valid plan");
        let image = PreparedImage {
            id: "sha256:1111".into(),
            repo_digests: vec!["python@sha256:2222".into()],
            os: "linux".into(),
            architecture: "amd64".into(),
        };
        let first = plan
            .identity(&image, TenantDomain::new("tenant-a").expect("valid tenant"))
            .expect("identity");
        let second = plan
            .identity(&image, TenantDomain::new("tenant-b").expect("valid tenant"))
            .expect("identity");
        assert_ne!(first, second);
        assert!(!format!("{first:?}").contains("python"));
    }
}
