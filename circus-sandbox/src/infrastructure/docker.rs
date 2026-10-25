//! Docker backend descriptor and adapter. Not a public SDK surface.

use std::future::Future;

use super::backend::{Backend, BackendProcess, Capabilities, CommonSpec, InstanceId, PrepareError};
use super::engine::{
    CreateSpec, Engine, EngineError, EngineProcess, ExecDone, HostReport, ProcessSpec, Recovered,
    SocketKind, UploadEntry, WorkspaceEntry,
};

/// Local Docker Engine endpoint. TCP, HTTP, and SSH are rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DockerEndpoint {
    /// Unix socket. `unix:///var/run/docker.sock` or `/var/run/docker.sock`.
    UnixSocket { path: String },
    /// Windows named pipe, including `npipe:////./pipe/dockerDesktopLinuxEngine`.
    NamedPipe { path: String },
}

impl DockerEndpoint {
    /// Parses a local socket or named-pipe endpoint.
    pub(crate) fn parse(value: &str) -> Result<Self, DockerConfigError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(DockerConfigError::UnsupportedEndpoint {
                value: value.to_string(),
            });
        }
        if value.starts_with("tcp://")
            || value.starts_with("http://")
            || value.starts_with("https://")
            || value.starts_with("ssh://")
            || value.starts_with("fd://")
        {
            return Err(DockerConfigError::UnsupportedEndpoint {
                value: value.to_string(),
            });
        }
        if let Some(rest) = value.strip_prefix("npipe://") {
            if !rest.starts_with("//./pipe/") || rest.len() == "//./pipe/".len() {
                return Err(DockerConfigError::UnsupportedEndpoint {
                    value: value.to_string(),
                });
            }
            return Ok(Self::NamedPipe {
                path: value.to_string(),
            });
        }
        if let Some(rest) = value.strip_prefix("unix://") {
            if !rest.starts_with('/') {
                return Err(DockerConfigError::UnsupportedEndpoint {
                    value: value.to_string(),
                });
            }
            return Ok(Self::UnixSocket {
                path: value.to_string(),
            });
        }
        if value.starts_with('/') {
            return Ok(Self::UnixSocket {
                path: format!("unix://{value}"),
            });
        }
        Err(DockerConfigError::UnsupportedEndpoint {
            value: value.to_string(),
        })
    }

    pub(crate) fn path(&self) -> &str {
        match self {
            Self::UnixSocket { path } | Self::NamedPipe { path } => path,
        }
    }

    pub(crate) fn socket_kind(&self) -> SocketKind {
        match self {
            Self::UnixSocket { .. } => SocketKind::Unix,
            Self::NamedPipe { .. } => SocketKind::Pipe,
        }
    }
}

/// OCI runtime name. Only `runc` is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeName;

impl RuntimeName {
    pub(crate) fn parse(name: &str) -> Result<Self, DockerConfigError> {
        if name == "runc" {
            Ok(Self)
        } else {
            Err(DockerConfigError::UnsupportedRuntime {
                name: name.to_string(),
            })
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        "runc"
    }
}

/// Docker-only create configuration. Kept out of the public API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerConfig {
    endpoint: DockerEndpoint,
    image: String,
    runtime: RuntimeName,
}

impl DockerConfig {
    /// Validates Docker endpoint, image, and runtime values.
    ///
    /// The image is not pulled. The endpoint must be an authorized local Unix
    /// socket or Windows named pipe.
    pub(crate) fn new(
        endpoint: &str,
        image: &str,
        runtime: &str,
    ) -> Result<Self, DockerConfigError> {
        let endpoint = DockerEndpoint::parse(endpoint)?;
        if image.is_empty() || image.chars().any(char::is_whitespace) {
            return Err(DockerConfigError::ImageRequired);
        }
        Ok(Self {
            endpoint,
            image: image.to_string(),
            runtime: RuntimeName::parse(runtime)?,
        })
    }

    /// Docker-only constraint: the PID budget includes init, a supervisor, and
    /// the command, so at least three PIDs are required.
    pub(crate) fn validate_common(&self, spec: &CommonSpec) -> Result<(), DockerConfigError> {
        let pids = u32::try_from(spec.pids).unwrap_or(u32::MAX);
        if pids < 3 {
            return Err(DockerConfigError::SupervisorNeedsPid { pids });
        }
        Ok(())
    }
}

/// Why Docker configuration was rejected before any container allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DockerConfigError {
    UnsupportedEndpoint { value: String },
    UnsupportedRuntime { name: String },
    ImageRequired,
    SupervisorNeedsPid { pids: u32 },
}

/// Real Docker Engine adapter implementing the private backend boundary.
pub(crate) struct DockerBackend {
    engine: Engine,
    config: DockerConfig,
}

impl DockerBackend {
    /// Connects and negotiates the Engine API version. Does not allocate.
    pub(crate) async fn connect(config: DockerConfig) -> Result<Self, Box<EngineError>> {
        let engine = Engine::connect(config.endpoint.socket_kind(), config.endpoint.path())
            .await
            .map_err(Box::new)?;
        Ok(Self { engine, config })
    }

    fn create_spec(&self, spec: &CommonSpec, nonce: &str) -> CreateSpec {
        CreateSpec {
            image: self.config.image.clone(),
            runtime: self.config.runtime.as_str().to_string(),
            memory: spec.memory,
            nano_cpus: spec.nano_cpus,
            pids: spec.pids,
            workspace_path: spec.workspace_path.clone(),
            workspace_bytes: spec.workspace_bytes,
            guest_uid: spec.guest_uid,
            guest_gid: spec.guest_gid,
            owner_label: nonce.to_string(),
        }
    }
}

// The trait returns explicit `impl Future + Send` bounds for static dispatch, so
// these implementations cannot use `async fn` syntax without erasing the Send
// guarantee that the lifecycle worker's spawned future depends on.
#[allow(clippy::manual_async_fn)]
impl Backend for DockerBackend {
    fn kind(&self) -> &'static str {
        "docker"
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
        let image = self.config.image.clone();
        async move {
            let result = self
                .engine
                .prepare(&image, self.config.runtime.as_str())
                .await
                .map_err(|error| {
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
        let spec = self.create_spec(spec, nonce);
        async move {
            self.engine
                .create_owned(name, &spec)
                .await
                .map(InstanceId::new)
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
        async move { self.engine.start(id.as_str()).await }
    }

    fn pause(&self, id: &InstanceId) -> impl Future<Output = Result<(), EngineError>> + Send {
        async move { self.engine.pause(id.as_str()).await }
    }

    fn resume(&self, id: &InstanceId) -> impl Future<Output = Result<(), EngineError>> + Send {
        async move { self.engine.resume(id.as_str()).await }
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
            // The attached exec identity stays engine-owned; the returned exit
            // future confirms the guest status after the output stream ends.
            let engine = self.engine.clone();
            let EngineProcess {
                id: exec,
                input,
                output,
            } = self.engine.spawn_process(id.as_str(), spec).await?;
            Ok(BackendProcess {
                input,
                output,
                exit: Box::pin(async move { engine.process_exit(&exec).await }),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(pids: i64) -> CommonSpec {
        CommonSpec {
            memory: 1024,
            nano_cpus: 1,
            pids,
            workspace_path: "/workspace".to_string(),
            workspace_bytes: 512,
            guest_uid: 1000,
            guest_gid: 1000,
        }
    }

    #[test]
    fn rejects_remote_endpoints_and_unknown_runtimes() {
        assert!(matches!(
            DockerConfig::new("tcp://127.0.0.1:2375", "image", "runc"),
            Err(DockerConfigError::UnsupportedEndpoint { .. })
        ));
        assert!(matches!(
            DockerConfig::new("npipe://remote/pipe/name", "image", "runc"),
            Err(DockerConfigError::UnsupportedEndpoint { .. })
        ));
        assert!(matches!(
            DockerConfig::new("unix:///var/run/docker.sock", "image", "runsc"),
            Err(DockerConfigError::UnsupportedRuntime { .. })
        ));
        assert!(matches!(
            DockerConfig::new("unix:///var/run/docker.sock", "  ", "runc"),
            Err(DockerConfigError::ImageRequired)
        ));
    }

    #[test]
    fn accepts_only_supported_endpoint_forms() {
        for value in [
            "unix:///var/run/docker.sock",
            "/var/run/docker.sock",
            "npipe:////./pipe/dockerDesktopLinuxEngine",
        ] {
            assert!(DockerConfig::new(value, "image:tag", "runc").is_ok());
        }
    }

    #[test]
    fn enforces_docker_supervisor_pid_floor() {
        let config = DockerConfig::new("unix:///var/run/docker.sock", "image", "runc").unwrap();
        assert!(matches!(
            config.validate_common(&spec(2)),
            Err(DockerConfigError::SupervisorNeedsPid { pids: 2 })
        ));
        assert!(config.validate_common(&spec(3)).is_ok());
    }
}
