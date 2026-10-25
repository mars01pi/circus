//! Backend connection shared by creation and orphan recovery.

use std::time::Duration;

#[cfg(all(unix, feature = "containerd"))]
use crate::infrastructure::ContainerdBackend;
#[cfg(all(unix, feature = "containerd"))]
use crate::infrastructure::ContainerdConfig;
use crate::infrastructure::{Backend, CommonSpec, DockerBackend, DockerConfig, Recovered};
#[cfg(feature = "kubernetes")]
use crate::infrastructure::{KubernetesBackend, KubernetesConfig};
use crate::runtime::BackendConfig;

#[cfg(all(unix, feature = "containerd"))]
use super::map_containerd_config;
#[cfg(feature = "kubernetes")]
use super::map_kubernetes_config;
use super::{Ownership, RecoveryId, Sandbox, SandboxError, engine_err, map_docker_config};

/// Connect deadline for orphan recovery, which is called without a
/// `SandboxConfig`. It bounds client construction only; the backend's
/// ownership check performs its own bounded inspection.
const ORPHAN_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// A connected backend whose concrete type depends on the caller's descriptor.
///
/// Create and orphan recovery share descriptor validation and connection, then
/// dispatch once to the statically dispatched backend operation they need.
pub(super) enum ConnectedBackend {
    Docker(DockerBackend),
    #[cfg(all(unix, feature = "containerd"))]
    Containerd(ContainerdBackend),
    #[cfg(feature = "kubernetes")]
    Kubernetes(KubernetesBackend),
}

/// Validates and connects only the selected backend.
///
/// `spec` is present for create, where common values are validated before any
/// allocation. Recovery passes `None`: it creates nothing, so it neither needs
/// nor fabricates common create values.
pub(super) async fn connect_backend(
    backend: BackendConfig,
    spec: Option<&CommonSpec>,
    lifecycle_timeout: Duration,
) -> Result<ConnectedBackend, SandboxError> {
    #[cfg(not(feature = "kubernetes"))]
    let _ = lifecycle_timeout;
    match backend {
        BackendConfig::Docker(descriptor) => {
            let docker = DockerConfig::new(
                descriptor.endpoint(),
                descriptor.image(),
                descriptor.runtime(),
            )
            .map_err(|err| SandboxError::BackendConfig(map_docker_config(err)))?;
            if let Some(spec) = spec {
                docker
                    .validate_common(spec)
                    .map_err(|err| SandboxError::BackendConfig(map_docker_config(err)))?;
            }
            let backend = DockerBackend::connect(docker)
                .await
                .map_err(|err| engine_err(*err))?;
            Ok(ConnectedBackend::Docker(backend))
        }
        BackendConfig::Containerd(descriptor) => {
            #[cfg(all(unix, feature = "containerd"))]
            {
                let containerd = ContainerdConfig::new(
                    descriptor.socket(),
                    descriptor.namespace(),
                    descriptor.image(),
                    descriptor.runtime(),
                    descriptor.snapshotter(),
                )
                .map_err(|err| SandboxError::BackendConfig(map_containerd_config(err)))?;
                let backend = ContainerdBackend::connect(containerd)
                    .await
                    .map_err(|err| engine_err(*err))?;
                Ok(ConnectedBackend::Containerd(backend))
            }
            #[cfg(not(all(unix, feature = "containerd")))]
            {
                let _ = descriptor;
                Err(SandboxError::UnsupportedCapability {
                    capability: "containerd backend feature not compiled",
                })
            }
        }
        BackendConfig::Kubernetes(descriptor) => {
            #[cfg(feature = "kubernetes")]
            {
                let kubernetes = KubernetesConfig::new(
                    descriptor.kubeconfig(),
                    descriptor.context(),
                    descriptor.namespace(),
                    descriptor.image(),
                    descriptor.service_account(),
                )
                .map_err(|err| SandboxError::BackendConfig(map_kubernetes_config(err)))?;
                let backend = match spec {
                    Some(spec) => {
                        kubernetes.validate_common(spec).map_err(|err| {
                            SandboxError::BackendConfig(map_kubernetes_config(err))
                        })?;
                        KubernetesBackend::connect(kubernetes, spec.clone(), lifecycle_timeout)
                            .await
                            .map_err(|err| engine_err(*err))?
                    }
                    None => KubernetesBackend::connect_for_recovery(kubernetes, lifecycle_timeout)
                        .await
                        .map_err(|err| engine_err(*err))?,
                };
                Ok(ConnectedBackend::Kubernetes(backend))
            }
            #[cfg(not(feature = "kubernetes"))]
            {
                let _ = descriptor;
                Err(SandboxError::UnsupportedCapability {
                    capability: "kubernetes backend feature not compiled",
                })
            }
        }
    }
}

impl Sandbox {
    /// Inspects and, when owned, removes an orphan left by a previous process.
    ///
    /// Only the backend selected by `backend` is connected: no image is
    /// prepared and no new instance is requested. The instance name is derived
    /// exclusively as `circus-sandbox-{recovery_id}`, and the backend must
    /// confirm that exact name carries the matching owner label before it
    /// removes anything. A name alone never authorizes deletion.
    ///
    /// The returned [`Ownership`] is best effort: `ConfirmedAbsent` means the
    /// backend observed removal or prior absence, while `StillPresent` and
    /// `Unknown` retain the possibility that an instance still exists. Backend
    /// diagnostics and owner labels are not included in the result. Callers
    /// that need a fresh sandbox after a confirmed absence should create one
    /// with the same persisted [`RecoveryId`].
    ///
    /// Because there is no in-process handle, this method takes no
    /// `SandboxConfig`; Kubernetes client construction uses a fixed connect
    /// deadline. Invalid IDs cannot reach this call because [`RecoveryId`] only
    /// exists in canonical form.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::BackendConfig`] for an invalid descriptor,
    /// [`SandboxError::UnsupportedCapability`] when the selected backend
    /// feature is not compiled, and [`SandboxError::Backend`] when connecting
    /// to the backend fails. An unconfirmed removal is reported as
    /// [`Ownership::Unknown`] rather than an error.
    pub async fn recover_orphan(
        backend: BackendConfig,
        recovery_id: &RecoveryId,
    ) -> Result<Ownership, SandboxError> {
        let connected = connect_backend(backend, None, ORPHAN_CONNECT_TIMEOUT).await?;
        Ok(match connected {
            ConnectedBackend::Docker(backend) => {
                recover_orphan_with_backend(&backend, recovery_id).await
            }
            #[cfg(all(unix, feature = "containerd"))]
            ConnectedBackend::Containerd(backend) => {
                recover_orphan_with_backend(&backend, recovery_id).await
            }
            #[cfg(feature = "kubernetes")]
            ConnectedBackend::Kubernetes(backend) => {
                recover_orphan_with_backend(&backend, recovery_id).await
            }
        })
    }
}

/// Runs the ownership-preserving recovery protocol for one recovery ID.
///
/// The backend receives exactly `circus-sandbox-{id}` as the name and the ID
/// itself as the owner label. No retained instance ID is supplied because the
/// caller has no in-process handle; an unconfirmed name is never deleted.
pub(super) async fn recover_orphan_with_backend<B: Backend>(
    backend: &B,
    recovery_id: &RecoveryId,
) -> Ownership {
    let name = format!("circus-sandbox-{recovery_id}");
    ownership_from_recovered(
        backend
            .recover_owned(&name, recovery_id.as_str(), None)
            .await,
    )
}

/// Maps the private recovery outcome to the public ownership model without
/// copying backend diagnostics.
fn ownership_from_recovered(recovered: Recovered) -> Ownership {
    match recovered {
        Recovered::Removed | Recovered::Absent => Ownership::ConfirmedAbsent,
        Recovered::StillPresent { .. } => Ownership::StillPresent,
        Recovered::Unknown { .. } => Ownership::Unknown,
    }
}
