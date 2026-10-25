//! Explicit image preparation and descriptor construction.
//!
//! Docker and containerd resolve an immutable identity before creation; a
//! Kubernetes image is left to the node kubelet. No registry credentials or
//! host defaults are consulted.

use circus_sandbox::BackendConfig;
use circus_sandbox::environment::{ImagePullPolicy, ImageRequest, prepare_docker_image};

use crate::config::{BackendPlan, ImageSettings};

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("image platform and preparation deadline are required for this backend")]
    MissingImageSettings,
    #[error("image is absent from the local image store and imagePullPolicy is local-only")]
    ImageUnavailable,
    #[error("image preparation failed")]
    ImagePreparation,
    #[error("containerd support is not compiled into this binary")]
    ContainerdUnavailable,
    #[error("Kubernetes support is not compiled into this binary")]
    KubernetesUnavailable,
}

impl BackendPlan {
    /// Rejects a backend whose support was not compiled into this binary
    /// before state or archive IO. Recovery cannot run without that support
    /// either, so this is fail-closed rather than a silent fallback.
    pub fn ensure_available(&self) -> Result<(), BackendError> {
        match self {
            Self::Docker { .. } => Ok(()),
            Self::Containerd { .. } if cfg!(all(unix, feature = "containerd")) => Ok(()),
            Self::Containerd { .. } => Err(BackendError::ContainerdUnavailable),
            Self::Kubernetes { .. } if cfg!(feature = "kubernetes") => Ok(()),
            Self::Kubernetes { .. } => Err(BackendError::KubernetesUnavailable),
        }
    }
}

impl BackendPlan {
    /// Resolves the backend descriptor with the configured pull policy.
    ///
    /// `LocalOnly` inspects only. `IfMissing` permits one unauthenticated pull
    /// after a local miss. Retrieval never happens before orphan recovery or
    /// archive validation.
    pub async fn prepare(&self, image: &ImageSettings) -> Result<BackendConfig, BackendError> {
        match self {
            Self::Docker {
                endpoint,
                image: reference,
                runtime,
            } => {
                let platform = image
                    .platform
                    .clone()
                    .ok_or(BackendError::MissingImageSettings)?;
                let timeout = image
                    .prepare_timeout
                    .ok_or(BackendError::MissingImageSettings)?;
                let request =
                    ImageRequest::new(endpoint.clone(), reference.clone(), platform, timeout)
                        .map_err(|_| BackendError::ImagePreparation)?
                        .with_pull_policy(image.pull_policy);
                let prepared = prepare_docker_image(request).await.map_err(|error| {
                    match (image.pull_policy, &error) {
                        (
                            ImagePullPolicy::LocalOnly,
                            circus_sandbox::environment::EnvironmentError::ImageNotFound { .. },
                        ) => BackendError::ImageUnavailable,
                        _ => BackendError::ImagePreparation,
                    }
                })?;
                Ok(BackendConfig::docker(
                    endpoint.as_str(),
                    prepared.id(),
                    runtime.as_str(),
                ))
            }
            Self::Containerd {
                socket,
                namespace,
                image: reference,
                runtime,
                snapshotter,
            } => {
                prepare_containerd(socket, namespace, reference, runtime, snapshotter, image).await
            }
            Self::Kubernetes { .. } => Ok(self.descriptor()),
        }
    }
}

#[cfg(all(unix, feature = "containerd"))]
async fn prepare_containerd(
    socket: &str,
    namespace: &str,
    reference: &str,
    runtime: &str,
    snapshotter: &str,
    settings: &ImageSettings,
) -> Result<BackendConfig, BackendError> {
    use circus_sandbox::environment::{ContainerdImageRequest, prepare_containerd_image};

    let platform = settings
        .platform
        .clone()
        .ok_or(BackendError::MissingImageSettings)?;
    let timeout = settings
        .prepare_timeout
        .ok_or(BackendError::MissingImageSettings)?;
    let request = ContainerdImageRequest::new(socket, namespace, reference, platform, timeout)
        .map_err(|_| BackendError::ImagePreparation)?
        .with_pull_policy(settings.pull_policy);
    let prepared = prepare_containerd_image(request).await.map_err(|error| {
        match (settings.pull_policy, &error) {
            (
                ImagePullPolicy::LocalOnly,
                circus_sandbox::environment::ContainerdImageError::ImageNotFound { .. },
            ) => BackendError::ImageUnavailable,
            _ => BackendError::ImagePreparation,
        }
    })?;
    Ok(BackendConfig::containerd(
        socket,
        namespace,
        prepared.reference(),
        runtime,
        snapshotter,
    ))
}

#[cfg(not(all(unix, feature = "containerd")))]
async fn prepare_containerd(
    _socket: &str,
    _namespace: &str,
    _reference: &str,
    _runtime: &str,
    _snapshotter: &str,
    _settings: &ImageSettings,
) -> Result<BackendConfig, BackendError> {
    Err(BackendError::ContainerdUnavailable)
}
