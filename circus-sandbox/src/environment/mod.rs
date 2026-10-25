//! Explicit OCI image resolution against local Docker and containerd daemons.
//!
//! Docker preparation is async and requires a Tokio runtime. An explicit local
//! endpoint, image reference, Linux platform, and whole-operation timeout are
//! required; no Docker context, environment credentials, or Docker
//! configuration authentication is loaded. Requests are local-only unless
//! [`ImagePullPolicy::IfMissing`] is explicitly selected. Only a missing local
//! image permits a pull; daemon failures and platform mismatches never do.
//! A successful result identifies inspected image contents, not a complete
//! dependency environment or a guarantee that sandbox creation will accept the
//! image (for example, the guest Python requirement still applies).
//!
//! Dropping the preparation future or reaching its deadline stops observing the
//! daemon request without spawning detached work. A pull may continue inside
//! Docker and mutate its shared cache even after failure or cancellation; this
//! crate never deletes shared images. Concurrent users may move tags or remove
//! images. Pass [`PreparedImage::id`] to [`crate::BackendConfig::docker`] to avoid
//! using a mutable tag; callers remain responsible for retaining the image.
//!
//! Unix builds with the `containerd` feature also expose separate containerd
//! request/result/error types and preparation. They require an explicit socket,
//! namespace, platform, and deadline; local-only may register immutable alias
//! metadata but never contacts a registry. See the containerd preparation API
//! for manifest/platform verification and daemon-transfer cancellation limits.
//!
//! [`EnvironmentPlan`] is a separate, explicitly ordered preparation plan. It
//! requires a pinned version for every step, executes each step as a separate
//! argv (never through a shell), records bounded output, and destroys its
//! sandbox with confirmed cleanup. [`EnvironmentId`] derives an opaque SHA-256
//! identity from the resolved image ID and repository digests, platform,
//! ordered pinned steps, and a caller-supplied tenant security domain. Identity
//! derivation has no cache, registry, persistence, or lookup behavior.

#[cfg(all(unix, feature = "containerd"))]
mod containerd;

#[cfg(all(unix, feature = "containerd"))]
pub use containerd::{
    ContainerdImageError, ContainerdImageRequest, PreparedContainerdImage, prepare_containerd_image,
};

mod identity;
mod plan;
mod sha256;

pub use identity::{EnvironmentId, IdentityError, TenantDomain};
pub use plan::{
    EnvironmentPlan, PinnedVersion, PlanError, PreparationError, PrepareStep, PreparedEnvironment,
    StepObservation,
};

use std::time::Duration;

use crate::infrastructure::{DockerEndpoint, Engine, EngineError, ImageInfo};

/// An explicit Linux OS, canonical Docker architecture, and optional variant.
///
/// Architecture names are Docker names such as `amd64` and `arm64`, not host
/// aliases such as `x86_64` or `aarch64`. No host-platform default is inferred.
/// [`ImagePlatform::new`] requests no variant; [`ImagePlatform::with_variant`]
/// adds an explicit one, sent during explicitly enabled image acquisition.
/// Docker inspection reports only OS/architecture, so its variant is carried
/// into an [`EnvironmentId`] but not verified. Containerd always verifies OS
/// and architecture. An unspecified variant accepts an inspected variant; an
/// explicit variant must match config or selected-index metadata. An absent
/// config variant may use the selected index's variant, but conflicting values
/// are rejected. No variant is inferred from the host; a direct manifest with
/// no variant evidence cannot satisfy an explicit variant request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePlatform {
    os: String,
    architecture: String,
    variant: Option<String>,
}

impl ImagePlatform {
    /// Validates an explicitly requested platform without a variant.
    ///
    /// # Errors
    ///
    /// Returns [`EnvironmentError::InvalidPlatform`] for a non-Linux OS or an
    /// unsupported Docker architecture. Use [`Self::with_variant`] to request a
    /// variant.
    pub fn new(
        os: impl Into<String>,
        architecture: impl Into<String>,
    ) -> Result<Self, EnvironmentError> {
        Self::with_variant(os, architecture, None)
    }

    /// Validates an explicitly requested platform and optional variant.
    ///
    /// # Errors
    ///
    /// Returns [`EnvironmentError::InvalidPlatform`] for a non-Linux OS or an
    /// unsupported Docker architecture, and
    /// [`EnvironmentError::InvalidPlatformVariant`] for an empty variant or one
    /// containing whitespace/control characters.
    pub fn with_variant(
        os: impl Into<String>,
        architecture: impl Into<String>,
        variant: Option<String>,
    ) -> Result<Self, EnvironmentError> {
        let os = os.into();
        let architecture = architecture.into();
        if os != "linux"
            || !matches!(
                architecture.as_str(),
                "386"
                    | "amd64"
                    | "arm"
                    | "arm64"
                    | "mips"
                    | "mipsle"
                    | "mips64"
                    | "mips64le"
                    | "ppc64"
                    | "ppc64le"
                    | "riscv64"
                    | "s390x"
            )
        {
            return Err(EnvironmentError::InvalidPlatform { os, architecture });
        }
        if variant.as_deref().is_some_and(|value| {
            value.is_empty()
                || value
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
        }) {
            return Err(EnvironmentError::InvalidPlatformVariant {
                variant: variant.unwrap_or_default(),
            });
        }
        Ok(Self {
            os,
            architecture,
            variant,
        })
    }

    /// Requested OS (always `linux`).
    #[must_use]
    pub fn os(&self) -> &str {
        &self.os
    }

    /// Canonical Docker architecture.
    #[must_use]
    pub fn architecture(&self) -> &str {
        &self.architecture
    }

    /// Explicit variant, if one was requested.
    #[must_use]
    pub fn variant(&self) -> Option<&str> {
        self.variant.as_deref()
    }
}

/// Registry access permitted during explicit image preparation.
///
/// This policy does not change sandbox creation, which requires an image
/// already present in the selected daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagePullPolicy {
    /// Inspect only the local cache; a missing image is a typed failure.
    LocalOnly,
    /// Pull without registry credentials, but only after a local image miss.
    IfMissing,
}

/// Validated immutable inputs for one image resolution operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRequest {
    endpoint: String,
    reference: String,
    platform: ImagePlatform,
    timeout: Duration,
    pull_policy: ImagePullPolicy,
}

impl ImageRequest {
    /// Validates all inputs without connecting to Docker.
    ///
    /// `endpoint` must name an authorized local Unix socket or Windows named
    /// pipe. `timeout` bounds connection/version negotiation, inspection, any
    /// explicitly enabled pull, and final inspection as a single operation.
    /// References must be nonempty and contain no whitespace/control characters;
    /// Docker performs the remaining reference syntax validation. The policy is
    /// [`ImagePullPolicy::LocalOnly`]; use [`Self::with_pull_policy`] to opt in.
    ///
    /// # Errors
    ///
    /// Returns [`EnvironmentError::InvalidEndpoint`],
    /// [`EnvironmentError::InvalidReference`], or
    /// [`EnvironmentError::InvalidTimeout`].
    pub fn new(
        endpoint: impl Into<String>,
        reference: impl Into<String>,
        platform: ImagePlatform,
        timeout: Duration,
    ) -> Result<Self, EnvironmentError> {
        let raw_endpoint = endpoint.into();
        let endpoint = DockerEndpoint::parse(&raw_endpoint).map_err(|_| {
            EnvironmentError::InvalidEndpoint {
                value: raw_endpoint,
            }
        })?;
        let reference = reference.into();
        if reference.is_empty()
            || reference
                .chars()
                .any(|value| value.is_whitespace() || value.is_control())
        {
            return Err(EnvironmentError::InvalidReference);
        }
        if timeout.is_zero() {
            return Err(EnvironmentError::InvalidTimeout);
        }
        Ok(Self {
            endpoint: endpoint.path().to_string(),
            reference,
            platform,
            timeout,
            pull_policy: ImagePullPolicy::LocalOnly,
        })
    }

    /// Selects whether a local image miss may contact a registry.
    ///
    /// [`ImagePullPolicy::IfMissing`] never refreshes a cached image, never
    /// retries daemon or platform failures, and does not load registry
    /// credentials. An untagged repository pulls only `latest`, not all tags.
    #[must_use]
    pub fn with_pull_policy(mut self, policy: ImagePullPolicy) -> Self {
        self.pull_policy = policy;
        self
    }

    /// Explicit image preparation pull policy.
    #[must_use]
    pub fn pull_policy(&self) -> ImagePullPolicy {
        self.pull_policy
    }

    /// Explicit local Docker endpoint.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Reference whose final contents will be inspected.
    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// Required OS and architecture, checked against the final image inspection.
    #[must_use]
    pub fn platform(&self) -> &ImagePlatform {
        &self.platform
    }

    /// Whole-operation deadline duration.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// Immutable contents and actual platform reported by the final image inspect.
///
/// This value does not own the daemon cache and does not prevent external image
/// removal. Repository digests can be empty for locally built images.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedImage {
    id: String,
    repo_digests: Vec<String>,
    os: String,
    architecture: String,
}

impl PreparedImage {
    /// Immutable image ID suitable for [`crate::BackendConfig::docker`].
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Repository digests returned by Docker, if available.
    #[must_use]
    pub fn repo_digests(&self) -> &[String] {
        &self.repo_digests
    }

    /// Actual inspected OS.
    #[must_use]
    pub fn os(&self) -> &str {
        &self.os
    }

    /// Actual inspected Docker architecture.
    #[must_use]
    pub fn architecture(&self) -> &str {
        &self.architecture
    }
}

/// A resolution failure. An attempted pull may have changed Docker's shared cache.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvironmentError {
    #[error("image reference must be nonempty without whitespace or control characters")]
    InvalidReference,
    #[error("image preparation timeout must be non-zero")]
    InvalidTimeout,
    #[error("local endpoint is not an authorized unix socket or npipe: {value}")]
    InvalidEndpoint { value: String },
    #[error(
        "unsupported image platform {os}/{architecture}; explicit Linux Docker architecture required"
    )]
    InvalidPlatform { os: String, architecture: String },
    #[error(
        "platform variant must be nonempty without whitespace or control characters: {variant}"
    )]
    InvalidPlatformVariant { variant: String },
    #[error("image {reference} is absent from the local daemon cache")]
    ImageNotFound { reference: String },
    #[error(
        "image platform {actual_os}/{actual_architecture} does not match requested {expected:?}"
    )]
    PlatformMismatch {
        expected: ImagePlatform,
        actual_os: String,
        actual_architecture: String,
    },
    #[error("Docker image preparation failed: {message}")]
    Engine { message: String },
    #[error("image preparation exceeded its whole-operation deadline")]
    Timeout,
}

/// Resolves an explicitly referenced image to its inspected immutable identity.
///
/// The timeout includes connection negotiation, local inspection, any pull,
/// and final inspection. The default [`ImagePullPolicy::LocalOnly`] returns
/// [`EnvironmentError::ImageNotFound`] for a cache miss.
/// [`ImagePullPolicy::IfMissing`] permits one unauthenticated pull after that
/// miss, using the requested Linux platform. Cached images are never refreshed;
/// daemon failures and platform mismatches never trigger a pull. No fallback
/// image is selected.
///
/// Dropping this future stops client observation without spawning detached
/// work. Docker may continue a pull and mutate its shared cache after failure,
/// timeout, or cancellation; no shared image is deleted. The returned ID and
/// OS/architecture come from final inspection, not pull progress messages.
///
/// # Errors
///
/// Returns [`EnvironmentError`] for endpoint/daemon or pull failures, a missing
/// local image, a platform mismatch, or expiration of the operation's timeout.
pub async fn prepare_docker_image(
    request: ImageRequest,
) -> Result<PreparedImage, EnvironmentError> {
    tokio::time::timeout(request.timeout, prepare(&request))
        .await
        .map_err(|_| EnvironmentError::Timeout)?
}

async fn prepare(request: &ImageRequest) -> Result<PreparedImage, EnvironmentError> {
    let endpoint = DockerEndpoint::parse(&request.endpoint).map_err(|_| {
        EnvironmentError::InvalidEndpoint {
            value: request.endpoint.clone(),
        }
    })?;
    let engine = Engine::connect(endpoint.socket_kind(), endpoint.path())
        .await
        .map_err(engine_error)?;
    let info: ImageInfo = match engine.inspect_image(&request.reference).await {
        Ok(info) => info,
        Err(error) if error.not_found && request.pull_policy == ImagePullPolicy::IfMissing => {
            let platform = match request.platform.variant() {
                Some(variant) => format!(
                    "{}/{}/{variant}",
                    request.platform.os, request.platform.architecture
                ),
                None => format!("{}/{}", request.platform.os, request.platform.architecture),
            };
            engine
                .pull_image(&request.reference, &platform)
                .await
                .map_err(engine_error)?;
            engine
                .inspect_image(&request.reference)
                .await
                .map_err(|error| image_error(request, error))?
        }
        Err(error) => return Err(image_error(request, error)),
    };
    if info.os != request.platform.os || info.architecture != request.platform.architecture {
        return Err(EnvironmentError::PlatformMismatch {
            expected: request.platform.clone(),
            actual_os: info.os,
            actual_architecture: info.architecture,
        });
    }
    Ok(PreparedImage {
        id: info.id,
        repo_digests: info.repo_digests,
        os: info.os,
        architecture: info.architecture,
    })
}

fn image_error(request: &ImageRequest, error: EngineError) -> EnvironmentError {
    if error.not_found {
        EnvironmentError::ImageNotFound {
            reference: request.reference.clone(),
        }
    } else {
        engine_error(error)
    }
}

fn engine_error(error: EngineError) -> EnvironmentError {
    EnvironmentError::Engine {
        message: error.message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_implicit_platforms_and_invalid_inputs() {
        assert!(ImagePlatform::new("windows", "amd64").is_err());
        assert!(ImagePlatform::new("linux", "x86_64").is_err());
        assert!(ImagePlatform::new("linux", "arm64/v8").is_err());
        let platform = ImagePlatform::new("linux", "amd64").unwrap();
        assert_eq!(platform.variant(), None);
        let variant =
            ImagePlatform::with_variant("linux", "arm64", Some("v8".to_string())).unwrap();
        assert_eq!(variant.variant(), Some("v8"));
        assert!(matches!(
            ImagePlatform::with_variant("linux", "arm64", Some(" ".to_string())),
            Err(EnvironmentError::InvalidPlatformVariant { .. })
        ));
        for reference in ["", " ", "image\ntag", "image\0tag"] {
            assert!(matches!(
                ImageRequest::new(
                    "unix:///var/run/docker.sock",
                    reference,
                    platform.clone(),
                    Duration::from_secs(1)
                ),
                Err(EnvironmentError::InvalidReference)
            ));
        }
        assert!(matches!(
            ImageRequest::new(
                "unix:///var/run/docker.sock",
                "image",
                platform.clone(),
                Duration::ZERO
            ),
            Err(EnvironmentError::InvalidTimeout)
        ));
        assert!(matches!(
            ImageRequest::new(
                "tcp://localhost:2375",
                "image",
                platform,
                Duration::from_secs(1)
            ),
            Err(EnvironmentError::InvalidEndpoint { .. })
        ));
    }
}
