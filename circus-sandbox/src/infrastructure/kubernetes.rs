//! Kubernetes workload adapter: static dispatch over the private `Backend`
//! trait. No Kubernetes SDK type crosses this module.

use std::future::Future;
use std::time::Duration;

use super::backend::{Backend, BackendProcess, Capabilities, CommonSpec, InstanceId, PrepareError};
use super::engine::{
    EngineError, EngineErrorKind, ExecDone, HostReport, ProcessSpec, Recovered, UploadEntry,
    WorkspaceEntry,
};
use super::kubernetes_config::KubernetesConfig;
use super::kubernetes_engine::KubernetesEngine;

/// Real Kubernetes workload adapter. Workspace snapshots reuse the shared
/// guest file transport; interactive processes attach over `pods/exec`.
/// Pause/resume is not implemented by this workload provider, and the
/// capability report makes the lifecycle reject it before allocation.
pub(crate) struct KubernetesBackend {
    engine: KubernetesEngine,
    config: KubernetesConfig,
    /// Retained for server-side dry-run validation during `prepare`, which has
    /// no arguments of its own. Absent on recovery-only connections.
    spec: Option<CommonSpec>,
}

impl KubernetesBackend {
    /// Loads an explicit kubeconfig and builds a namespace-scoped client.
    ///
    /// The `CommonSpec` is retained for server-side dry-run validation during
    /// `prepare`, which has no arguments of its own.
    pub(crate) async fn connect(
        config: KubernetesConfig,
        spec: CommonSpec,
        lifecycle_timeout: Duration,
    ) -> Result<Self, Box<EngineError>> {
        Self::connect_with_spec(config, Some(spec), lifecycle_timeout).await
    }

    /// Connects only enough to inspect and remove an orphan Pod.
    ///
    /// No image preparation or Pod creation is possible on this connection;
    /// `prepare` reports that it is unavailable.
    pub(crate) async fn connect_for_recovery(
        config: KubernetesConfig,
        lifecycle_timeout: Duration,
    ) -> Result<Self, Box<EngineError>> {
        Self::connect_with_spec(config, None, lifecycle_timeout).await
    }

    async fn connect_with_spec(
        config: KubernetesConfig,
        spec: Option<CommonSpec>,
        lifecycle_timeout: Duration,
    ) -> Result<Self, Box<EngineError>> {
        let engine = KubernetesEngine::connect(&config, lifecycle_timeout).await?;
        Ok(Self {
            engine,
            config,
            spec,
        })
    }
}

// The trait returns explicit `impl Future + Send` bounds for static dispatch.
#[allow(clippy::manual_async_fn)]
impl Backend for KubernetesBackend {
    fn kind(&self) -> &'static str {
        "kubernetes"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            pause_resume: false,
            file_transport: true,
            workspace_snapshot: true,
            spawn_process: true,
        }
    }

    fn prepare(&self) -> impl Future<Output = Result<(), PrepareError>> + Send {
        async move {
            let Some(spec) = self.spec.as_ref() else {
                return Err(PrepareError::Engine(
                    super::kubernetes_engine::engine_error(
                        EngineErrorKind::Other,
                        "Kubernetes backend was connected for recovery only",
                    ),
                ));
            };
            self.engine
                .prepare(spec, self.config.service_account())
                .await
        }
    }

    fn create_owned(
        &self,
        name: &str,
        spec: &CommonSpec,
        nonce: &str,
    ) -> impl Future<Output = Result<InstanceId, EngineError>> + Send {
        async move {
            self.engine
                .create_owned(name, spec, nonce, self.config.service_account())
                .await
        }
    }

    fn recover_owned(
        &self,
        name: &str,
        nonce: &str,
        _retained_id: Option<&InstanceId>,
    ) -> impl Future<Output = Recovered> + Send {
        // A retained id never authorizes deletion: only the live Pod's owner
        // labels can prove ownership.
        async move { self.engine.recover_owned(name, nonce).await }
    }

    fn start(&self, id: &InstanceId) -> impl Future<Output = Result<(), EngineError>> + Send {
        async move { self.engine.start(id.as_str()).await }
    }

    fn pause(&self, _id: &InstanceId) -> impl Future<Output = Result<(), EngineError>> + Send {
        async {
            Err(super::kubernetes_engine::engine_error(
                EngineErrorKind::Other,
                "this workload provider cannot pause a Pod",
            ))
        }
    }

    fn resume(&self, _id: &InstanceId) -> impl Future<Output = Result<(), EngineError>> + Send {
        async {
            Err(super::kubernetes_engine::engine_error(
                EngineErrorKind::Other,
                "this workload provider cannot resume a Pod",
            ))
        }
    }

    fn upload(
        &self,
        id: &InstanceId,
        user: &str,
        workspace: &str,
        entry: UploadEntry,
        max_bytes: u64,
    ) -> impl Future<Output = Result<(), EngineError>> + Send {
        async move {
            self.engine
                .upload(id.as_str(), user, workspace, entry, max_bytes)
                .await
        }
    }

    fn download_file(
        &self,
        id: &InstanceId,
        user: &str,
        workspace: &str,
        relative: &str,
        max_bytes: usize,
    ) -> impl Future<Output = Result<Vec<u8>, EngineError>> + Send {
        async move {
            self.engine
                .download_file(id.as_str(), user, workspace, relative, max_bytes)
                .await
        }
    }

    fn remove_file(
        &self,
        id: &InstanceId,
        user: &str,
        workspace: &str,
        relative: &str,
    ) -> impl Future<Output = Result<(), EngineError>> + Send {
        async move {
            self.engine
                .remove_file(id.as_str(), user, workspace, relative)
                .await
        }
    }

    fn exec(
        &self,
        id: &InstanceId,
        user: &str,
        workdir: &str,
        argv: &[String],
        output_limit: usize,
    ) -> impl Future<Output = Result<ExecDone, EngineError>> + Send {
        async move {
            self.engine
                .exec(id.as_str(), user, workdir, argv, output_limit)
                .await
        }
    }

    fn spawn_process(
        &self,
        id: &InstanceId,
        spec: ProcessSpec<'_>,
    ) -> impl Future<Output = Result<BackendProcess, EngineError>> + Send {
        async move { self.engine.spawn_process(id.as_str(), spec).await }
    }

    fn host_report(
        &self,
        id: &InstanceId,
    ) -> impl Future<Output = Result<HostReport, EngineError>> + Send {
        async move { self.engine.host_report(id.as_str()).await }
    }

    fn list_workspace(
        &self,
        id: &InstanceId,
        user: &str,
        workspace: &str,
        max_bytes: u64,
        max_entries: usize,
    ) -> impl Future<Output = Result<Vec<WorkspaceEntry>, EngineError>> + Send {
        async move {
            self.engine
                .list_workspace(id.as_str(), user, workspace, max_bytes, max_entries)
                .await
        }
    }
}
