//! Explicit image preparation in one local containerd namespace.

use std::time::Duration;

use super::{ImagePlatform, ImagePullPolicy};

/// Validated inputs for containerd image preparation; no ambient configuration.
#[derive(Clone, PartialEq, Eq)]
pub struct ContainerdImageRequest {
    socket: String,
    namespace: String,
    reference: String,
    platform: ImagePlatform,
    timeout: Duration,
    pull_policy: ImagePullPolicy,
}

impl std::fmt::Debug for ContainerdImageRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContainerdImageRequest")
            .field("platform", &self.platform)
            .field("timeout", &self.timeout)
            .field("pull_policy", &self.pull_policy)
            .finish_non_exhaustive()
    }
}

impl ContainerdImageRequest {
    /// Validates explicit inputs without contacting the daemon.
    ///
    /// `socket` is an absolute Unix path, not a URL. Namespace characters are
    /// ASCII letters/digits, `.`, `_`, and `-`, as in runtime configuration.
    /// References have no scheme, whitespace, control characters, or userinfo.
    /// Remaining reference syntax is validated by containerd. No host platform,
    /// environment variables, or registry credentials are inferred.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerdImageError::InvalidSocket`],
    /// [`ContainerdImageError::InvalidNamespace`],
    /// [`ContainerdImageError::InvalidReference`], or
    /// [`ContainerdImageError::InvalidTimeout`].
    pub fn new(
        socket: impl Into<String>,
        namespace: impl Into<String>,
        reference: impl Into<String>,
        platform: ImagePlatform,
        timeout: Duration,
    ) -> Result<Self, ContainerdImageError> {
        let socket = socket.into();
        if !socket.starts_with('/')
            || socket.starts_with("//")
            || socket.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(ContainerdImageError::InvalidSocket);
        }
        let namespace = namespace.into();
        if namespace.is_empty()
            || !namespace
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(ContainerdImageError::InvalidNamespace);
        }
        let reference = reference.into();
        // A digest separator is allowed, but user:password@registry is not.
        if reference.is_empty()
            || reference.contains("://")
            || reference.contains(['\\', '?', '#'])
            || reference
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
            || reference.split_once('@').is_some_and(|(_, digest)| {
                !digest.split_once(':').is_some_and(|(algorithm, encoded)| {
                    let length = match algorithm {
                        "sha256" => 64,
                        "sha512" => 128,
                        _ => return false,
                    };
                    encoded.len() == length && encoded.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            })
        {
            return Err(ContainerdImageError::InvalidReference);
        }
        if timeout.is_zero() || std::time::Instant::now().checked_add(timeout).is_none() {
            return Err(ContainerdImageError::InvalidTimeout);
        }
        Ok(Self {
            socket,
            namespace,
            reference,
            platform,
            timeout,
            pull_policy: ImagePullPolicy::LocalOnly,
        })
    }

    /// Selects whether a missing local record permits one unauthenticated transfer.
    #[must_use]
    pub fn with_pull_policy(mut self, policy: ImagePullPolicy) -> Self {
        self.pull_policy = policy;
        self
    }

    /// Explicit local Unix socket path.
    #[must_use]
    pub fn socket(&self) -> &str {
        &self.socket
    }
    /// Namespace used for lookup, transfer, and immutable alias registration.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    /// Caller-selected image reference.
    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }
    /// Explicit requested platform, including an optional variant.
    #[must_use]
    pub fn platform(&self) -> &ImagePlatform {
        &self.platform
    }
    /// Whole-operation timeout.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
    /// Registry access policy (defaults to local-only).
    #[must_use]
    pub fn pull_policy(&self) -> ImagePullPolicy {
        self.pull_policy
    }
}

/// Selected manifest identity and verified platform in the requested namespace.
///
/// The reference is a digest-qualified image-record alias suitable for
/// [`crate::BackendConfig::containerd`]. It does not retain or own shared cache
/// entries, prevent external removal, or promise host-platform executability.
/// The alias does not retain index-only variant evidence. If a source index
/// reports `v8` but its selected config omits variant, preparing the source with
/// explicit `v8` succeeds; preparing the direct alias with explicit `v8`
/// returns [`ContainerdImageError::PlatformMismatch`]. Repeat the original
/// source request to revalidate that evidence. The alias remains directly
/// usable by the backend; no provenance annotations are persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedContainerdImage {
    reference: String,
    digest: String,
    platform: ImagePlatform,
}

impl PreparedContainerdImage {
    pub(crate) fn new(reference: String, digest: String, platform: ImagePlatform) -> Self {
        Self {
            reference,
            digest,
            platform,
        }
    }
    /// Immutable digest-qualified alias in the selected namespace.
    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }
    /// Digest of the selected manifest, not an index or image configuration ID.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }
    /// Platform verified from selected manifest/config metadata.
    #[must_use]
    pub fn platform(&self) -> &ImagePlatform {
        &self.platform
    }
}

/// Preparation failure; a daemon transfer/cache change may outlive this error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContainerdImageError {
    #[error("containerd socket must be an explicit absolute Unix path")]
    InvalidSocket,
    #[error("containerd namespace must be nonempty ASCII letters/digits or ._-")]
    InvalidNamespace,
    #[error("image reference must have no scheme, credentials, whitespace, or controls")]
    InvalidReference,
    #[error("image preparation timeout must be non-zero and representable")]
    InvalidTimeout,
    #[error("image {reference} is absent from the selected containerd namespace")]
    ImageNotFound { reference: String },
    #[error("image platform {actual:?} does not match requested {expected:?}")]
    PlatformMismatch {
        expected: Box<ImagePlatform>,
        actual: Option<ImagePlatform>,
    },
    #[error("immutable image alias {reference} already targets different contents")]
    AliasConflict { reference: String },
    #[error("containerd image preparation failed: {message}")]
    Daemon { message: String },
    #[error("image preparation exceeded its whole-operation deadline")]
    Timeout,
}

/// Prepares an image through containerd's local gRPC API.
///
/// The deadline covers connection, lookup, optional transfer, metadata/platform
/// verification, and immutable alias registration. Local-only never accesses a
/// registry, but may register that alias. IfMissing permits transfer only after
/// a missing image record, never for daemon failures or platform mismatches.
/// Existing aliases must match digest, media type, and size; annotations are
/// not content identity. Aliases are never overwritten. A direct-manifest alias
/// does not retain index-only variant evidence; repeat the original source
/// request when that evidence is needed for explicit variant verification.
/// No registry credentials, retries, cache deletion, or detached client work are
/// introduced. A daemon transfer/cache mutation can persist after cancellation
/// or timeout. This does not create a sandbox or unpack its filesystem.
///
/// # Errors
///
/// Returns [`ContainerdImageError`] on cache misses, platform/alias conflicts,
/// daemon/transfer/metadata failures, or expiry of the whole-operation timeout.
pub async fn prepare_containerd_image(
    request: ContainerdImageRequest,
) -> Result<PreparedContainerdImage, ContainerdImageError> {
    use crate::infrastructure::{ContainerdImageFailure, ContainerdImageSpec, ContainerdPlatform};

    let spec = ContainerdImageSpec {
        socket: request.socket(),
        namespace: request.namespace(),
        reference: request.reference(),
        platform: ContainerdPlatform {
            os: request.platform().os(),
            architecture: request.platform().architecture(),
            variant: request.platform().variant(),
        },
        pull_if_missing: request.pull_policy() == ImagePullPolicy::IfMissing,
    };
    let prepared = tokio::time::timeout(
        request.timeout(),
        crate::infrastructure::prepare_containerd_image(&spec),
    )
    .await
    .map_err(|_| ContainerdImageError::Timeout)?
    .map_err(|error| match error {
        ContainerdImageFailure::ImageNotFound => ContainerdImageError::ImageNotFound {
            reference: request.reference().to_string(),
        },
        ContainerdImageFailure::PlatformMismatch { actual } => {
            ContainerdImageError::PlatformMismatch {
                expected: Box::new(request.platform().clone()),
                actual: actual.and_then(|platform| {
                    ImagePlatform::with_variant(
                        platform.os,
                        platform.architecture,
                        platform.variant,
                    )
                    .ok()
                }),
            }
        }
        ContainerdImageFailure::AliasConflict { reference } => {
            ContainerdImageError::AliasConflict { reference }
        }
        ContainerdImageFailure::Daemon { message } => ContainerdImageError::Daemon { message },
    })?;
    let platform = ImagePlatform::with_variant(
        prepared.platform.os,
        prepared.platform.architecture,
        prepared.platform.variant,
    )
    .map_err(|_| ContainerdImageError::Daemon {
        message: "containerd returned an unsupported verified platform".to_string(),
    })?;
    Ok(PreparedContainerdImage::new(
        prepared.reference,
        prepared.digest,
        platform,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        socket: &str,
        namespace: &str,
        reference: &str,
        timeout: Duration,
    ) -> Result<ContainerdImageRequest, ContainerdImageError> {
        ContainerdImageRequest::new(
            socket,
            namespace,
            reference,
            ImagePlatform::new("linux", "amd64").unwrap(),
            timeout,
        )
    }

    #[test]
    fn validates_socket_namespace_reference_and_timeout() {
        let timeout = Duration::from_secs(10);
        for socket in [
            "",
            "relative.sock",
            "unix:///run/containerd.sock",
            "//host/socket",
            "/run/a\n",
            "/run/a\0",
        ] {
            assert_eq!(
                request(socket, "default", "example/image:tag", timeout),
                Err(ContainerdImageError::InvalidSocket)
            );
        }
        for namespace in ["", "a/b", "has space", "名前", "a\n"] {
            assert_eq!(
                request("/run/containerd.sock", namespace, "image:tag", timeout),
                Err(ContainerdImageError::InvalidNamespace)
            );
        }
        for reference in [
            "",
            "image tag",
            "image\0",
            "image\n",
            "https://registry/image",
            "user:password@registry/image",
            "user:password@registry/image@sha256:abc",
            "user:password@registry",
            "user:password@host:port",
            "image@sha256:abc",
            "image\\tag",
            "image?token=secret",
            "image#fragment",
        ] {
            assert_eq!(
                request("/run/containerd.sock", "default", reference, timeout),
                Err(ContainerdImageError::InvalidReference)
            );
        }
        assert_eq!(
            request("/run/containerd.sock", "default", "image", Duration::ZERO),
            Err(ContainerdImageError::InvalidTimeout)
        );
        assert_eq!(
            request("/run/containerd.sock", "default", "image", Duration::MAX),
            Err(ContainerdImageError::InvalidTimeout)
        );
    }

    #[test]
    fn debug_omits_endpoint_namespace_and_image_reference() {
        let platform = ImagePlatform::with_variant("linux", "arm64", Some("v8".into())).unwrap();
        let reference = format!("registry:5000/team/image@sha256:{}", "a".repeat(64));
        let request = ContainerdImageRequest::new(
            "/run/containerd.sock",
            "tenant_1.test-prod",
            reference,
            platform,
            Duration::from_secs(42),
        )
        .unwrap();
        let debug = format!("{request:?}");
        assert!(!debug.contains(request.socket()));
        assert!(!debug.contains(request.reference()));
        assert!(!debug.contains(request.namespace()));
    }
}
