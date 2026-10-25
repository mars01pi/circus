//! containerd backend adapter: static dispatch over the private `Backend`
//! trait. No containerd SDK type crosses this module.

use std::future::Future;

use super::backend::{Backend, BackendProcess, Capabilities, CommonSpec, InstanceId, PrepareError};
use super::containerd_config::ContainerdConfig;
use super::containerd_engine::{ContainerdEngine, EngineProcess};
use super::engine::{
    EngineError, ExecDone, HostReport, ProcessSpec, Recovered, UploadEntry, WorkspaceEntry,
    backend_transition, engine_error_kind,
};
use crate::observability::events;

/// Real containerd adapter over the CRI-independent gRPC API.
pub(crate) struct ContainerdBackend {
    engine: ContainerdEngine,
    config: ContainerdConfig,
}

impl ContainerdBackend {
    /// Connects to the configured socket and verifies the version service.
    ///
    /// Does not create containers, snapshots, or tasks.
    pub(crate) async fn connect(config: ContainerdConfig) -> Result<Self, Box<EngineError>> {
        let engine = ContainerdEngine::connect(
            config.socket(),
            config.namespace(),
            config.snapshotter(),
            config.image(),
        )
        .await
        .map_err(Box::new)?;
        Ok(Self { engine, config })
    }
}

// The trait returns explicit `impl Future + Send` bounds for static dispatch.
#[allow(clippy::manual_async_fn)]
impl Backend for ContainerdBackend {
    fn kind(&self) -> &'static str {
        "containerd"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            pause_resume: true,
            file_transport: true,
            workspace_snapshot: true,
            spawn_process: true,
        }
    }

    fn prepare(&self) -> impl Future<Output = Result<(), PrepareError>> + Send {
        let image = self.config.image().to_string();
        async move {
            let result = self.engine.prepare().await.map_err(|error| {
                if error.not_found {
                    PrepareError::ImageUnavailable { image }
                } else {
                    PrepareError::Engine(error)
                }
            });
            let outcome = match &result {
                Ok(()) => "ok",
                Err(PrepareError::ImageUnavailable { .. }) => "image_unavailable",
                Err(PrepareError::Engine(_)) => "engine_error",
            };
            crate::observability::events::backend_prepare_completed(outcome);
            result
        }
    }

    fn create_owned(
        &self,
        name: &str,
        spec: &CommonSpec,
        nonce: &str,
    ) -> impl Future<Output = Result<InstanceId, EngineError>> + Send {
        async move {
            let span = events::backend_create_span();
            let result = self
                .engine
                .create_owned(name, spec, nonce)
                .await
                .map(InstanceId::new);
            let (outcome, create_pending, error_kind) = match &result {
                Ok(_) => ("ok", false, "none"),
                Err(error) => ("error", error.create_pending, engine_error_kind(error)),
            };
            {
                let _entered = span.enter();
                events::backend_create_completed(outcome, create_pending, error_kind);
            }
            result
        }
    }

    fn recover_owned(
        &self,
        name: &str,
        nonce: &str,
        retained_id: Option<&InstanceId>,
    ) -> impl Future<Output = Recovered> + Send {
        async move {
            self.engine
                .recover_owned(name, nonce, retained_id.map(InstanceId::as_str))
                .await
        }
    }

    fn start(&self, id: &InstanceId) -> impl Future<Output = Result<(), EngineError>> + Send {
        async move {
            let result = self.engine.start(id.as_str()).await;
            backend_transition("start", &result);
            result
        }
    }

    fn pause(&self, id: &InstanceId) -> impl Future<Output = Result<(), EngineError>> + Send {
        async move {
            let result = self.engine.pause(id.as_str()).await;
            backend_transition("pause", &result);
            result
        }
    }

    fn resume(&self, id: &InstanceId) -> impl Future<Output = Result<(), EngineError>> + Send {
        async move {
            let result = self.engine.resume(id.as_str()).await;
            backend_transition("resume", &result);
            result
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
        async move {
            let EngineProcess {
                input,
                output,
                exit,
            } = self.engine.spawn_process(id.as_str(), spec).await?;
            Ok(BackendProcess {
                input,
                output,
                exit,
            })
        }
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
