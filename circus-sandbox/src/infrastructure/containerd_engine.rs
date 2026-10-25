//! containerd gRPC operations for one sandbox instance.
//!
//! Every client is built from the shared channel; the namespace is attached to
//! each request explicitly. The lifecycle owner remains the only state machine:
//! this module creates, transitions, transports, inspects, and removes the
//! resources it is told about, and reports ownership outcomes it observed.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use containerd_client::services::v1::{
    Container, CreateContainerRequest, CreateTaskRequest, DeleteContainerRequest,
    DeleteProcessRequest, DeleteTaskRequest, ExecProcessRequest, GetContainerRequest, KillRequest,
    PauseTaskRequest, ResumeTaskRequest, StartRequest, WaitRequest,
    container::Runtime,
    containers_client::ContainersClient,
    snapshots::{
        PrepareSnapshotRequest, RemoveSnapshotRequest, StatSnapshotRequest,
        snapshots_client::SnapshotsClient,
    },
    tasks_client::TasksClient,
    version_client::VersionClient,
};
use containerd_client::tonic::transport::Channel;
use containerd_client::tonic::{Code, Request};
use containerd_client::types::Mount;
use containerd_client::with_namespace;
use futures_util::{Stream, StreamExt, stream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};

use super::backend::CommonSpec;
use super::containerd_grpc::{
    connect_error, local_error, map_status, with_ownership, without_ownership,
};
use super::containerd_image::ensure_rootfs;
// containerd mutates one shared snapshot chain for every sandbox created from
// the same image. Container creation and ownership-checked recovery therefore
// serialize per process so a recovery cannot remove a snapshot while another
// sandbox is mounting or initializing its rootfs.
static SNAPSHOT_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

use super::containerd_io::{
    CloseIo, FifoDir, FifoWriter, open_reader_async, open_writer, unblock_readers,
};
use super::containerd_spec::{container_any, process_any, report_from_any};
use super::engine::{
    EngineError, EngineErrorKind, ExecDone, HostReport, OWNER_LABEL_KEY, OutputCollector,
    OutputPhase, ProcessChunk, ProcessSpec, Recovered, UploadEntry, UploadSource, WorkspaceEntry,
    decode_manifest, file_error,
};
use super::guest_transport::{
    ExecIo, GuestFileMode, GuestStdin, TRANSFER_CHUNK_BYTES, helper_argv, helper_failure,
    helper_limits,
};

const RUNC_HANDLER: &str = "io.containerd.runc.v2";
const CONTAINER_HOSTNAME: &str = "circus-sandbox";
const RPC_TIMEOUT: Duration = Duration::from_secs(60);
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const SIGKILL: u32 = 9;
const CHUNK_BYTES: usize = 64 * 1024;
const OUTPUT_CHUNKS: usize = 32;

/// One attached interactive process for containerd.
pub(crate) struct EngineProcess {
    pub input: Pin<Box<dyn tokio::io::AsyncWrite + Send>>,
    pub output: Pin<Box<dyn Stream<Item = Result<ProcessChunk, EngineError>> + Send>>,
    pub exit: Pin<Box<dyn Future<Output = Result<i64, EngineError>> + Send>>,
}

#[derive(Clone)]
pub(crate) struct ContainerdEngine {
    channel: Channel,
    namespace: String,
    snapshotter: String,
    image: String,
}

impl ContainerdEngine {
    pub(crate) async fn connect(
        socket: &str,
        namespace: &str,
        snapshotter: &str,
        image: &str,
    ) -> Result<Self, EngineError> {
        let channel = containerd_client::connect(socket)
            .await
            .map_err(connect_error)?;
        let mut client = VersionClient::new(channel.clone());
        let mut request = Request::new(());
        request.set_timeout(RPC_TIMEOUT);
        client.version(request).await.map_err(map_status)?;
        Ok(Self {
            channel,
            namespace: namespace.to_string(),
            snapshotter: snapshotter.to_string(),
            image: image.to_string(),
        })
    }

    pub(crate) async fn prepare(&self) -> Result<(), EngineError> {
        super::containerd_image::image_exists(&self.channel, &self.namespace, &self.image).await
    }

    pub(crate) async fn create_owned(
        &self,
        name: &str,
        spec: &CommonSpec,
        nonce: &str,
    ) -> Result<String, EngineError> {
        let _snapshot = SNAPSHOT_LOCK.lock().await;
        let (parent, image_env) = ensure_rootfs(
            &self.channel,
            &self.namespace,
            &self.snapshotter,
            &self.image,
        )
        .await
        .map_err(without_ownership)?;
        let container_spec =
            container_any(spec, CONTAINER_HOSTNAME, &image_env).map_err(without_ownership)?;
        let labels = owner_labels(name, nonce);
        let mounts = self
            .prepare_snapshot(name, &parent, &labels)
            .await
            .map_err(|error| with_ownership(error, Some(name.to_string())))?;
        let identity = Some(name.to_string());
        let mut client = ContainersClient::new(self.channel.clone());
        let mut request = with_namespace!(
            CreateContainerRequest {
                container: Some(Container {
                    id: name.to_string(),
                    labels: labels.clone(),
                    image: self.image.clone(),
                    runtime: Some(Runtime {
                        name: RUNC_HANDLER.to_string(),
                        options: None,
                    }),
                    spec: Some(container_spec),
                    snapshotter: self.snapshotter.clone(),
                    snapshot_key: name.to_string(),
                    ..Default::default()
                }),
            },
            self.namespace.as_str()
        );
        request.set_timeout(RPC_TIMEOUT);
        if let Err(status) = client.create(request).await {
            return Err(with_ownership(map_status(status), identity));
        }
        if let Err(error) = self.create_task(name, mounts).await {
            return Err(with_ownership(error, identity));
        }
        Ok(name.to_string())
    }

    async fn prepare_snapshot(
        &self,
        key: &str,
        parent: &str,
        labels: &HashMap<String, String>,
    ) -> Result<Vec<Mount>, EngineError> {
        let mut client = SnapshotsClient::new(self.channel.clone());
        let mut request = with_namespace!(
            PrepareSnapshotRequest {
                snapshotter: self.snapshotter.clone(),
                key: key.to_string(),
                parent: parent.to_string(),
                labels: labels.clone(),
            },
            self.namespace.as_str()
        );
        request.set_timeout(RPC_TIMEOUT);
        Ok(client
            .prepare(request)
            .await
            .map_err(map_status)?
            .into_inner()
            .mounts)
    }

    async fn create_task(&self, name: &str, rootfs: Vec<Mount>) -> Result<(), EngineError> {
        let mut client = TasksClient::new(self.channel.clone());
        let mut request = with_namespace!(
            CreateTaskRequest {
                container_id: name.to_string(),
                rootfs,
                ..Default::default()
            },
            self.namespace.as_str()
        );
        request.set_timeout(RPC_TIMEOUT);
        client.create(request).await.map_err(map_status)?;
        Ok(())
    }

    pub(crate) async fn start(&self, id: &str) -> Result<(), EngineError> {
        self.task_start(id, "").await.map(|_| ())
    }

    pub(crate) async fn pause(&self, id: &str) -> Result<(), EngineError> {
        let mut client = TasksClient::new(self.channel.clone());
        let mut request = with_namespace!(
            PauseTaskRequest {
                container_id: id.to_string(),
            },
            self.namespace.as_str()
        );
        request.set_timeout(RPC_TIMEOUT);
        client.pause(request).await.map_err(map_status)?;
        Ok(())
    }

    pub(crate) async fn resume(&self, id: &str) -> Result<(), EngineError> {
        let mut client = TasksClient::new(self.channel.clone());
        let mut request = with_namespace!(
            ResumeTaskRequest {
                container_id: id.to_string(),
            },
            self.namespace.as_str()
        );
        request.set_timeout(RPC_TIMEOUT);
        client.resume(request).await.map_err(map_status)?;
        Ok(())
    }

    async fn task_start(&self, id: &str, exec_id: &str) -> Result<u32, EngineError> {
        let mut client = TasksClient::new(self.channel.clone());
        let mut request = with_namespace!(
            StartRequest {
                container_id: id.to_string(),
                exec_id: exec_id.to_string(),
            },
            self.namespace.as_str()
        );
        request.set_timeout(RPC_TIMEOUT);
        Ok(client
            .start(request)
            .await
            .map_err(map_status)?
            .into_inner()
            .pid)
    }

    pub(crate) async fn exec(
        &self,
        id: &str,
        user: &str,
        workdir: &str,
        argv: &[String],
        output_limit: usize,
    ) -> Result<ExecDone, EngineError> {
        self.exec_input(
            id,
            user,
            workdir,
            argv,
            &[],
            ExecIo {
                stdin: GuestStdin::None,
                stdout_limit: output_limit,
                stderr_limit: output_limit,
                combined_limit: output_limit,
            },
        )
        .await
    }

    async fn file_helper(
        &self,
        id: &str,
        user: &str,
        workspace: &str,
        request: FileOp<'_>,
    ) -> Result<Vec<u8>, EngineError> {
        let FileOp {
            relative,
            mode,
            stdin,
            max_bytes,
            max_entries,
        } = request;
        let bound = if mode == GuestFileMode::Write {
            stdin.bound()
        } else {
            u64::try_from(max_bytes).unwrap_or(u64::MAX)
        };
        let argv = helper_argv(workspace, relative, mode, bound, max_entries);
        let (stdout_limit, stderr_limit, combined_limit) = helper_limits(max_bytes);
        let result = self
            .exec_input(
                id,
                user,
                "/",
                &argv,
                &[],
                ExecIo {
                    stdin,
                    stdout_limit,
                    stderr_limit,
                    combined_limit,
                },
            )
            .await?;
        if result.exit_code == 0 {
            Ok(result.stdout)
        } else {
            Err(helper_failure(result.exit_code))
        }
    }

    async fn exec_input(
        &self,
        id: &str,
        user: &str,
        workdir: &str,
        argv: &[String],
        env: &[String],
        io: ExecIo<'_>,
    ) -> Result<ExecDone, EngineError> {
        let container = self.get_container(id).await?;
        let spec = process_any(user, workdir, argv, env, container.spec.as_ref())?;
        let fifo = FifoDir::create()?;
        let exec_id = format!("exec-{}", uuid::Uuid::new_v4());
        let stdin_path = if io.stdin.attached() {
            fifo.stdin()
        } else {
            String::new()
        };
        let mut client = TasksClient::new(self.channel.clone());
        let mut request = with_namespace!(
            ExecProcessRequest {
                container_id: id.to_string(),
                stdin: stdin_path,
                stdout: fifo.stdout(),
                stderr: fifo.stderr(),
                terminal: false,
                spec: Some(spec),
                exec_id: exec_id.clone(),
            },
            self.namespace.as_str()
        );
        request.set_timeout(RPC_TIMEOUT);
        client.exec(request).await.map_err(map_status)?;

        let writer = if io.stdin.attached() {
            Some(open_writer(
                &fifo.stdin_path(),
                CloseIo::new(self.channel.clone(), &self.namespace, id, &exec_id),
            )?)
        } else {
            None
        };
        let stdout_task = tokio::spawn(open_reader_async(fifo.stdout_path()));
        let stderr_task = tokio::spawn(open_reader_async(fifo.stderr_path()));
        if let Err(error) = self.task_start(id, &exec_id).await {
            unblock_readers([&fifo.stdout_path(), &fifo.stderr_path()]);
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            self.delete_process_best_effort(id, &exec_id).await;
            return Err(error);
        }
        let stdout = stdout_task
            .await
            .map_err(|err| local_error(format!("stdout reader task: {err}")))??;
        let stderr = stderr_task
            .await
            .map_err(|err| local_error(format!("stderr reader task: {err}")))??;

        let collector = Arc::new(Mutex::new(OutputCollector::new(
            io.stdout_limit,
            io.stderr_limit,
            io.combined_limit,
        )));
        let mut send = Box::pin(send_stdin(io.stdin, writer));
        let mut send_done = false;
        let mut send_error = None;
        let mut pumps = futures_util::stream::FuturesUnordered::new();
        pumps.push(pump(stdout, OutputPhase::Stdout, Arc::clone(&collector)));
        pumps.push(pump(stderr, OutputPhase::Stderr, Arc::clone(&collector)));
        let mut output_error = None;
        while !send_done || !pumps.is_empty() {
            tokio::select! {
                result = &mut send, if !send_done => {
                    send_done = true;
                    send_error = result.err();
                }
                result = pumps.next(), if !pumps.is_empty() => {
                    if let Some(Err(error)) = result {
                        self.kill_exec(id, &exec_id).await;
                        output_error.get_or_insert(error);
                    }
                }
            }
        }
        let exit_code = self.wait_exec(id, &exec_id).await?;
        self.delete_process_best_effort(id, &exec_id).await;
        if let Some(error) = output_error {
            return Err(error);
        }
        if exit_code == 0
            && let Some(error) = send_error
        {
            return Err(error);
        }
        let collector = Arc::try_unwrap(collector)
            .map_err(|_| local_error("containerd exec collector still shared"))?
            .into_inner()
            .unwrap_or_else(|err| err.into_inner());
        let (stdout, stderr) = collector.finish();
        Ok(ExecDone {
            exit_code,
            stdout,
            stderr,
        })
    }

    async fn wait_exec(&self, id: &str, exec_id: &str) -> Result<i64, EngineError> {
        let mut client = TasksClient::new(self.channel.clone());
        let request = with_namespace!(
            WaitRequest {
                container_id: id.to_string(),
                exec_id: exec_id.to_string(),
            },
            self.namespace.as_str()
        );
        let response = client.wait(request).await.map_err(map_status)?.into_inner();
        Ok(i64::from(response.exit_status))
    }

    async fn kill_exec(&self, id: &str, exec_id: &str) {
        let mut client = TasksClient::new(self.channel.clone());
        let request = with_namespace!(
            KillRequest {
                container_id: id.to_string(),
                exec_id: exec_id.to_string(),
                signal: SIGKILL,
                all: false,
            },
            self.namespace.as_str()
        );
        let _ = client.kill(request).await;
    }

    async fn delete_process_best_effort(&self, id: &str, exec_id: &str) {
        let mut client = TasksClient::new(self.channel.clone());
        let request = with_namespace!(
            DeleteProcessRequest {
                container_id: id.to_string(),
                exec_id: exec_id.to_string(),
            },
            self.namespace.as_str()
        );
        let _ = client.delete_process(request).await;
    }

    pub(crate) async fn spawn_process(
        &self,
        id: &str,
        spec: ProcessSpec<'_>,
    ) -> Result<EngineProcess, EngineError> {
        let container = self.get_container(id).await?;
        let process = process_any(
            spec.user,
            spec.workdir,
            spec.argv,
            &spec.env,
            container.spec.as_ref(),
        )?;
        let fifo = FifoDir::create()?;
        let exec_id = format!("exec-{}", uuid::Uuid::new_v4());
        let mut client = TasksClient::new(self.channel.clone());
        let mut request = with_namespace!(
            ExecProcessRequest {
                container_id: id.to_string(),
                stdin: fifo.stdin(),
                stdout: fifo.stdout(),
                stderr: fifo.stderr(),
                terminal: false,
                spec: Some(process),
                exec_id: exec_id.clone(),
            },
            self.namespace.as_str()
        );
        request.set_timeout(RPC_TIMEOUT);
        client.exec(request).await.map_err(map_status)?;
        let input = open_writer(
            &fifo.stdin_path(),
            CloseIo::new(self.channel.clone(), &self.namespace, id, &exec_id),
        )?;
        let stdout_task = tokio::spawn(open_reader_async(fifo.stdout_path()));
        let stderr_task = tokio::spawn(open_reader_async(fifo.stderr_path()));
        if let Err(error) = self.task_start(id, &exec_id).await {
            unblock_readers([&fifo.stdout_path(), &fifo.stderr_path()]);
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            self.delete_process_best_effort(id, &exec_id).await;
            return Err(error);
        }
        let stdout = stdout_task
            .await
            .map_err(|err| local_error(format!("stdout reader task: {err}")))??;
        let stderr = stderr_task
            .await
            .map_err(|err| local_error(format!("stderr reader task: {err}")))??;
        let guard = fifo.into_guard();
        let (chunk_tx, chunk_rx) = mpsc::channel(OUTPUT_CHUNKS);
        let (done_tx, done_rx) = oneshot::channel();
        tokio::spawn(async move {
            let outputs = futures_util::future::join(
                pump_stream(stdout, ProcessChunk::Stdout, chunk_tx.clone()),
                pump_stream(stderr, ProcessChunk::Stderr, chunk_tx.clone()),
            );
            drop(chunk_tx);
            let _ = outputs.await;
            drop(guard);
            let _ = done_tx.send(());
        });
        let output = stream::unfold(chunk_rx, |mut receiver| async move {
            receiver.recv().await.map(|item| (item, receiver))
        });
        let engine = self.clone();
        let container = id.to_string();
        let exit_id = exec_id.clone();
        let exit = async move {
            let _ = done_rx.await;
            let status = engine.wait_exec(&container, &exit_id).await?;
            engine
                .delete_process_best_effort(&container, &exit_id)
                .await;
            Ok(status)
        };
        Ok(EngineProcess {
            input: Box::pin(input),
            output: Box::pin(output),
            exit: Box::pin(exit),
        })
    }

    pub(crate) async fn upload(
        &self,
        id: &str,
        user: &str,
        workspace: &str,
        entry: UploadEntry,
        max_bytes: u64,
    ) -> Result<(), EngineError> {
        let UploadEntry { path, source } = entry;
        match source {
            UploadSource::Directory => {
                self.file_helper(
                    id,
                    user,
                    workspace,
                    FileOp {
                        relative: &path,
                        mode: GuestFileMode::Mkdir,
                        stdin: GuestStdin::None,
                        max_bytes: 0,
                        max_entries: None,
                    },
                )
                .await?;
            }
            UploadSource::Bytes(bytes) => {
                if bytes.len() as u64 > max_bytes {
                    return Err(file_error(
                        EngineErrorKind::Limit,
                        "staged bytes exceed workspace limit",
                    ));
                }
                self.file_helper(
                    id,
                    user,
                    workspace,
                    FileOp {
                        relative: &path,
                        mode: GuestFileMode::Write,
                        stdin: GuestStdin::Bytes(&bytes),
                        max_bytes: 0,
                        max_entries: None,
                    },
                )
                .await?;
            }
            UploadSource::File { file, len } => {
                if len > max_bytes {
                    return Err(file_error(
                        EngineErrorKind::Limit,
                        "staged bytes exceed workspace limit",
                    ));
                }
                self.file_helper(
                    id,
                    user,
                    workspace,
                    FileOp {
                        relative: &path,
                        mode: GuestFileMode::Write,
                        stdin: GuestStdin::File { file, len },
                        max_bytes: 0,
                        max_entries: None,
                    },
                )
                .await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn download_file(
        &self,
        id: &str,
        user: &str,
        workspace: &str,
        relative: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, EngineError> {
        let bytes = self
            .file_helper(
                id,
                user,
                workspace,
                FileOp {
                    relative,
                    mode: GuestFileMode::Read,
                    stdin: GuestStdin::None,
                    max_bytes,
                    max_entries: None,
                },
            )
            .await?;
        if bytes.len() > max_bytes {
            return Err(file_error(
                EngineErrorKind::Limit,
                "file exceeds byte limit",
            ));
        }
        Ok(bytes)
    }

    pub(crate) async fn remove_file(
        &self,
        id: &str,
        user: &str,
        workspace: &str,
        relative: &str,
    ) -> Result<(), EngineError> {
        self.file_helper(
            id,
            user,
            workspace,
            FileOp {
                relative,
                mode: GuestFileMode::Unlink,
                stdin: GuestStdin::None,
                max_bytes: 0,
                max_entries: None,
            },
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn list_workspace(
        &self,
        id: &str,
        user: &str,
        workspace: &str,
        max_bytes: u64,
        max_entries: usize,
    ) -> Result<Vec<WorkspaceEntry>, EngineError> {
        if max_bytes == 0 {
            return Err(file_error(
                EngineErrorKind::Limit,
                "workspace manifest byte limit must be positive",
            ));
        }
        let bytes = self
            .file_helper(
                id,
                user,
                workspace,
                FileOp {
                    relative: "",
                    mode: GuestFileMode::List,
                    stdin: GuestStdin::None,
                    max_bytes: usize::try_from(max_bytes).unwrap_or(usize::MAX),
                    max_entries: Some(max_entries),
                },
            )
            .await?;
        decode_manifest(&bytes)
    }

    async fn get_container(&self, id: &str) -> Result<Container, EngineError> {
        let mut client = ContainersClient::new(self.channel.clone());
        let mut request = with_namespace!(
            GetContainerRequest { id: id.to_string() },
            self.namespace.as_str()
        );
        request.set_timeout(RPC_TIMEOUT);
        client
            .get(request)
            .await
            .map_err(map_status)?
            .into_inner()
            .container
            .ok_or_else(|| local_error("containerd get returned no container"))
    }

    pub(crate) async fn host_report(&self, id: &str) -> Result<HostReport, EngineError> {
        let container = self.get_container(id).await?;
        let runtime = container.runtime.map(|runtime| runtime.name);
        Ok(report_from_any(runtime, container.spec.as_ref()))
    }

    /// Deletes exactly the container and snapshot keyed by `name` when they
    /// carry this sandbox's owner label, then confirms absence with a fresh
    /// `Get`/`Stat`.
    pub(crate) async fn recover_owned(
        &self,
        name: &str,
        nonce: &str,
        retained_id: Option<&str>,
    ) -> Recovered {
        let _snapshot = SNAPSHOT_LOCK.lock().await;
        let container = self.container_check(name, nonce).await;
        let snapshot = self.snapshot_check(name, nonce).await;
        if matches!(container, OwnerCheck::Foreign) || matches!(snapshot, OwnerCheck::Foreign) {
            return Recovered::Unknown {
                detail: format!(
                    "{name} exists without the sandbox owner label; refusing to remove it"
                ),
            };
        }
        if matches!(container, OwnerCheck::Absent) && matches!(snapshot, OwnerCheck::Absent) {
            return Recovered::Absent;
        }
        let container_id = match &container {
            OwnerCheck::Owned(id) => Some(id.clone()),
            OwnerCheck::Unknown { .. } => retained_id.map(str::to_string),
            OwnerCheck::Absent | OwnerCheck::Foreign => None,
        };
        let attempt_snapshot = match &snapshot {
            OwnerCheck::Owned(_) => true,
            OwnerCheck::Unknown { .. } => retained_id.is_some(),
            OwnerCheck::Absent | OwnerCheck::Foreign => false,
        };
        if container_id.is_none() && !attempt_snapshot {
            return Recovered::Unknown {
                detail: format!("neither ownership nor a retained id could confirm {name}"),
            };
        }
        let mut notes = Vec::new();
        if let Some(id) = &container_id {
            if let Err(error) = self.kill_task(id).await {
                notes.push(format!("stop failed: {}", error.message));
            }
            let stopped = match self.wait_task_stopped(id).await {
                Ok(()) => true,
                Err(error) => {
                    notes.push(format!("task did not stop: {}", error.message));
                    false
                }
            };
            if stopped {
                if let Err(error) = self.delete_task(id).await {
                    notes.push(format!("task removal failed: {}", error.message));
                }
                if let Err(error) = self.delete_container(id).await {
                    notes.push(format!("container removal failed: {}", error.message));
                }
            }
        } else {
            // An orphan task can outlive container metadata after a failed
            // earlier cleanup; removing it here is safe only because the name
            // is generated and no container with it exists.
            let _ = self.delete_task(name).await;
        }
        if attempt_snapshot && let Err(error) = self.remove_snapshot(name).await {
            notes.push(format!("snapshot removal failed: {}", error.message));
        }
        let container = self.container_check(name, nonce).await;
        let snapshot = self.snapshot_check(name, nonce).await;
        match (container, snapshot) {
            (OwnerCheck::Absent, OwnerCheck::Absent) => Recovered::Removed,
            (OwnerCheck::Unknown { message }, _) | (_, OwnerCheck::Unknown { message }) => {
                notes.push(format!("post-removal inspection failed: {message}"));
                Recovered::Unknown {
                    detail: notes.join("; "),
                }
            }
            (OwnerCheck::Owned(_), _) | (_, OwnerCheck::Owned(_)) => Recovered::StillPresent {
                detail: if notes.is_empty() {
                    "containerd still reports owned resources after removal".to_string()
                } else {
                    notes.join("; ")
                },
            },
            (OwnerCheck::Foreign, _) | (_, OwnerCheck::Foreign) => Recovered::Unknown {
                detail: format!("{name} no longer carries the sandbox owner label"),
            },
        }
    }

    async fn container_check(&self, name: &str, nonce: &str) -> OwnerCheck {
        let mut client = ContainersClient::new(self.channel.clone());
        let request = with_namespace!(
            GetContainerRequest {
                id: name.to_string(),
            },
            self.namespace.as_str()
        );
        match client.get(request).await {
            Ok(response) => match response.into_inner().container {
                Some(container) => {
                    if container.labels.get(OWNER_LABEL_KEY).map(String::as_str) == Some(nonce) {
                        OwnerCheck::Owned(container.id)
                    } else {
                        OwnerCheck::Foreign
                    }
                }
                None => OwnerCheck::Unknown {
                    message: "get returned no container".to_string(),
                },
            },
            Err(status) if status.code() == Code::NotFound => OwnerCheck::Absent,
            Err(status) => OwnerCheck::Unknown {
                message: status.message().to_string(),
            },
        }
    }

    async fn snapshot_check(&self, key: &str, nonce: &str) -> OwnerCheck {
        let mut client = SnapshotsClient::new(self.channel.clone());
        let request = with_namespace!(
            StatSnapshotRequest {
                snapshotter: self.snapshotter.clone(),
                key: key.to_string(),
            },
            self.namespace.as_str()
        );
        match client.stat(request).await {
            Ok(response) => match response.into_inner().info {
                Some(info) => {
                    if info.labels.get(OWNER_LABEL_KEY).map(String::as_str) == Some(nonce) {
                        OwnerCheck::Owned(info.name)
                    } else {
                        OwnerCheck::Foreign
                    }
                }
                None => OwnerCheck::Unknown {
                    message: "snapshot stat returned no info".to_string(),
                },
            },
            Err(status) if status.code() == Code::NotFound => OwnerCheck::Absent,
            Err(status) => OwnerCheck::Unknown {
                message: status.message().to_string(),
            },
        }
    }

    async fn kill_task(&self, id: &str) -> Result<(), EngineError> {
        let mut client = TasksClient::new(self.channel.clone());
        let mut request = with_namespace!(
            KillRequest {
                container_id: id.to_string(),
                exec_id: String::new(),
                signal: SIGKILL,
                all: true,
            },
            self.namespace.as_str()
        );
        request.set_timeout(WAIT_TIMEOUT);
        match client.kill(request).await {
            Ok(_) => Ok(()),
            Err(status) if status.code() == Code::NotFound => Ok(()),
            Err(status) => Err(map_status(status)),
        }
    }

    /// Waits until the init task is stopped. `DeleteTask` rejects a running
    /// task, and deleting the container metadata first would orphan its shim.
    async fn wait_task_stopped(&self, id: &str) -> Result<(), EngineError> {
        let mut client = TasksClient::new(self.channel.clone());
        let request = with_namespace!(
            WaitRequest {
                container_id: id.to_string(),
                exec_id: String::new(),
            },
            self.namespace.as_str()
        );
        match tokio::time::timeout(WAIT_TIMEOUT, client.wait(request)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(status)) if status.code() == Code::NotFound => Ok(()),
            Ok(Err(status)) => Err(map_status(status)),
            Err(_) => Err(local_error("task did not stop within the wait deadline")),
        }
    }

    async fn delete_task(&self, id: &str) -> Result<(), EngineError> {
        let mut client = TasksClient::new(self.channel.clone());
        let mut request = with_namespace!(
            DeleteTaskRequest {
                container_id: id.to_string(),
            },
            self.namespace.as_str()
        );
        request.set_timeout(WAIT_TIMEOUT);
        match client.delete(request).await {
            Ok(_) => Ok(()),
            Err(status) if status.code() == Code::NotFound => Ok(()),
            Err(status) => Err(map_status(status)),
        }
    }

    async fn delete_container(&self, id: &str) -> Result<(), EngineError> {
        let mut client = ContainersClient::new(self.channel.clone());
        let mut request = with_namespace!(
            DeleteContainerRequest { id: id.to_string() },
            self.namespace.as_str()
        );
        request.set_timeout(RPC_TIMEOUT);
        match client.delete(request).await {
            Ok(_) => Ok(()),
            Err(status) if status.code() == Code::NotFound => Ok(()),
            Err(status) => Err(map_status(status)),
        }
    }

    async fn remove_snapshot(&self, key: &str) -> Result<(), EngineError> {
        let mut client = SnapshotsClient::new(self.channel.clone());
        let mut request = with_namespace!(
            RemoveSnapshotRequest {
                snapshotter: self.snapshotter.clone(),
                key: key.to_string(),
            },
            self.namespace.as_str()
        );
        request.set_timeout(RPC_TIMEOUT);
        match client.remove(request).await {
            Ok(_) => Ok(()),
            Err(status) if status.code() == Code::NotFound => Ok(()),
            Err(status) => Err(map_status(status)),
        }
    }
}

fn owner_labels(name: &str, nonce: &str) -> HashMap<String, String> {
    HashMap::from([
        (OWNER_LABEL_KEY.to_string(), nonce.to_string()),
        ("circus.sandbox.name".to_string(), name.to_string()),
    ])
}

enum OwnerCheck {
    Owned(String),
    Absent,
    Foreign,
    Unknown { message: String },
}

/// One guest file-helper request.
struct FileOp<'a> {
    relative: &'a str,
    mode: GuestFileMode,
    stdin: GuestStdin<'a>,
    max_bytes: usize,
    max_entries: Option<usize>,
}

async fn send_stdin(stdin: GuestStdin<'_>, writer: Option<FifoWriter>) -> Result<(), EngineError> {
    let Some(mut writer) = writer else {
        return Ok(());
    };
    match stdin {
        GuestStdin::None => {}
        GuestStdin::Bytes(bytes) => {
            writer
                .write_all(bytes)
                .await
                .map_err(|err| local_error(format!("guest stdin write: {err}")))?;
        }
        GuestStdin::File { file, len } => {
            let mut source = tokio::fs::File::from_std(file);
            let mut chunk = vec![0u8; TRANSFER_CHUNK_BYTES];
            let mut remaining = len;
            while remaining > 0 {
                let want = usize::try_from(remaining)
                    .unwrap_or(TRANSFER_CHUNK_BYTES)
                    .min(chunk.len());
                let read = source.read(&mut chunk[..want]).await.map_err(|err| {
                    file_error(EngineErrorKind::FileIo, format!("staged file read: {err}"))
                })?;
                if read == 0 {
                    return Err(file_error(
                        EngineErrorKind::FileIo,
                        format!("staged file ended before its declared {len} byte bound"),
                    ));
                }
                writer
                    .write_all(&chunk[..read])
                    .await
                    .map_err(|err| local_error(format!("guest stdin write: {err}")))?;
                remaining -= u64::try_from(read).unwrap_or(0);
            }
        }
    }
    writer
        .shutdown()
        .await
        .map_err(|err| local_error(format!("guest stdin close: {err}")))?;
    drop(writer);
    Ok(())
}

async fn pump(
    mut file: tokio::fs::File,
    phase: OutputPhase,
    collector: Arc<Mutex<OutputCollector>>,
) -> Result<(), EngineError> {
    let mut buffer = vec![0u8; CHUNK_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|err| local_error(format!("containerd exec read: {err}")))?;
        if read == 0 {
            return Ok(());
        }
        let mut collector = collector.lock().unwrap_or_else(|err| err.into_inner());
        collector.push(phase, &buffer[..read])?;
    }
}

async fn pump_stream(
    mut file: tokio::fs::File,
    wrap: fn(Vec<u8>) -> ProcessChunk,
    sender: mpsc::Sender<Result<ProcessChunk, EngineError>>,
) -> Result<(), EngineError> {
    let mut buffer = vec![0u8; CHUNK_BYTES];
    loop {
        match file.read(&mut buffer).await {
            Ok(0) => return Ok(()),
            Ok(read) => {
                if sender
                    .send(Ok(wrap(buffer[..read].to_vec())))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
            Err(err) => {
                let _ = sender
                    .send(Err(local_error(format!("containerd exec read: {err}"))))
                    .await;
                return Err(local_error("containerd exec read failed"));
            }
        }
    }
}
