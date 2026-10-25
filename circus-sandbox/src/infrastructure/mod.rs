//! Docker deployment adapter. Not a public SDK surface.

mod backend;
#[cfg(all(unix, feature = "containerd"))]
mod containerd;
#[cfg(all(unix, feature = "containerd"))]
mod containerd_config;
#[cfg(all(unix, feature = "containerd"))]
mod containerd_engine;
#[cfg(all(unix, feature = "containerd"))]
mod containerd_grpc;
#[cfg(all(unix, feature = "containerd"))]
mod containerd_image;
#[cfg(all(unix, feature = "containerd"))]
mod containerd_io;
#[cfg(all(unix, feature = "containerd"))]
mod containerd_prepare;
#[cfg(all(unix, feature = "containerd"))]
mod containerd_spec;
mod docker;
mod engine;
pub(crate) mod git_process;
mod guest_transport;
#[cfg(feature = "kubernetes")]
mod kubernetes;
#[cfg(feature = "kubernetes")]
mod kubernetes_config;
#[cfg(feature = "kubernetes")]
mod kubernetes_engine;
#[cfg(feature = "kubernetes")]
mod kubernetes_io;
#[cfg(feature = "kubernetes")]
mod kubernetes_pod;
#[cfg(feature = "kubernetes")]
mod kubernetes_process;

#[cfg(test)]
pub(crate) use backend::Capabilities;
pub(crate) use backend::{Backend, BackendProcess, CommonSpec, InstanceId, PrepareError};
#[cfg(unix)]
#[cfg(all(unix, feature = "containerd"))]
pub(crate) use containerd::ContainerdBackend;
#[cfg(all(unix, feature = "containerd"))]
pub(crate) use containerd_config::{ContainerdConfig, ContainerdConfigError};
#[cfg(all(unix, feature = "containerd"))]
pub(crate) use containerd_prepare::{
    ContainerdImageFailure, ContainerdImageSpec, ContainerdPlatform, prepare_containerd_image,
};
pub(crate) use docker::{DockerBackend, DockerConfig, DockerConfigError, DockerEndpoint};
pub(crate) use engine::{
    Engine, EngineError, EngineErrorKind, ImageInfo, OutputPhase, ProcessChunk, ProcessSpec,
    Recovered, UploadEntry, UploadSource, WorkspaceEntry, WorkspaceEntryKind,
};
#[cfg(test)]
pub(crate) use engine::{ExecDone, HostReport, OutputLimit, PolicyMismatch};
#[cfg(feature = "kubernetes")]
pub(crate) use kubernetes::KubernetesBackend;
#[cfg(feature = "kubernetes")]
pub(crate) use kubernetes_config::{KubernetesConfig, KubernetesConfigError};
