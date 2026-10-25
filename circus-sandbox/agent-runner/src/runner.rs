use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use circus_sandbox::{
    CodeSnapshot, IoLimits, IsolationConfig, ProcessExit, ProcessRequest, ProcessSession, Sandbox,
    SandboxConfig, SandboxError, SandboxPhase, SnapshotError,
};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::{MAX_ARTIFACT_BYTES, PROCESS_BYTES, project_path};
use crate::events::{EVENT_QUEUE, Events};
use crate::model::CleanupOwner;
use crate::{
    AgentKind, CleanupHandle, RunnerConfig, TaskError, TaskEvent, TaskFailure, TaskOutcome,
    TaskOutput, TaskPhase, TaskRequest, TaskWorkspace,
};

pub(crate) const BRIDGE: &str = include_str!("codex_bridge.py");
pub(crate) const MCP_HOST: &str = include_str!("mcp.py");
const MCP_GUEST: &str = include_str!("mcp_guest.py");

/// Reusable caller configuration. Every `start` owns a fresh, isolated sandbox.
#[derive(Debug, Clone)]
pub struct AgentRunner {
    config: Arc<RunnerConfig>,
    agent: AgentKind,
    mcp_connections: Arc<Vec<crate::McpConnection>>,
}

impl AgentRunner {
    pub fn new(config: RunnerConfig) -> Result<Self, TaskError> {
        if config.python.as_os_str().is_empty() {
            return Err(TaskError::Invalid("host Python executable"));
        }
        Ok(Self {
            config: Arc::new(config),
            agent: AgentKind::Codex,
            mcp_connections: Arc::new(Vec::new()),
        })
    }

    /// Select a fixed agent adapter; the supplied backend image must contain its CLI.
    /// Codex uses Responses; Claude Code uses the Anthropic Messages API.
    pub fn with_agent(mut self, agent: AgentKind) -> Self {
        self.agent = agent;
        self
    }

    /// Validates before allocation and starts an owned task on the current Tokio
    /// runtime. Consume `next_event` concurrently with `wait`; the event queue is
    /// bounded and does not block cancellation or cleanup.
    pub fn start(&self, request: TaskRequest) -> Result<AgentTask, TaskError> {
        self.start_with_mcp(request, Vec::new())
    }

    /// Configure trusted endpoints separately from untrusted task requests.
    pub fn with_mcp_connections(
        mut self,
        connections: Vec<crate::McpConnection>,
    ) -> Result<Self, TaskError> {
        crate::mcp::validate_connections(&connections)?;
        self.mcp_connections = Arc::new(connections);
        Ok(self)
    }

    /// Explicit task grants never implicitly authorize every configured tool.
    pub fn start_with_mcp(
        &self,
        request: TaskRequest,
        grants: Vec<crate::McpGrant>,
    ) -> Result<AgentTask, TaskError> {
        let mcp_config = crate::mcp::task_config(&self.mcp_connections, &grants)?;
        self.start_inner(request, self.agent.host(), mcp_config)
    }

    #[cfg(test)]
    pub(crate) fn start_controller(
        &self,
        request: TaskRequest,
        controller: String,
    ) -> Result<AgentTask, TaskError> {
        self.start_inner(request, controller, "[]".into())
    }

    fn start_inner(
        &self,
        request: TaskRequest,
        controller: String,
        mcp_config: String,
    ) -> Result<AgentTask, TaskError> {
        request.validate(self.config.resources.max_file_bytes())?;
        if request.agent_session.is_some() && self.agent != AgentKind::Codex {
            return Err(TaskError::Invalid(
                "agent session resume is unsupported for this agent",
            ));
        }
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| TaskError::NoRuntime)?;
        let id = uuid::Uuid::new_v4().to_string();
        let cancel = CancellationToken::new();
        let (sender, receiver) = mpsc::channel(EVENT_QUEUE);
        let (outcome_tx, outcome) = watch::channel(None);
        let task = AgentTask {
            id: id.clone(),
            cancel: cancel.clone(),
            events: Mutex::new(receiver),
            outcome,
        };
        let config = self.config.clone();
        let guest = self.agent.guest();
        runtime.spawn(async move {
            let mut events = Events::new(sender);
            let result = run(
                &id,
                &config,
                request,
                &controller,
                &guest,
                &mcp_config,
                &cancel,
                &mut events,
            )
            .await;
            let _ = outcome_tx.send(Some(Arc::new(result)));
        });
        Ok(task)
    }
}

/// One owned task. `wait` is repeatable and cancellation-safe. Dropping the task
/// requests cancellation, while its worker retains cleanup responsibility.
pub struct AgentTask {
    id: String,
    cancel: CancellationToken,
    events: Mutex<mpsc::Receiver<TaskEvent>>,
    outcome: watch::Receiver<Option<Arc<TaskOutcome>>>,
}

impl AgentTask {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Idempotent request. Await `wait` to observe termination and cleanup.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Ordered progress, text and tool events. `None` means the worker closed the
    /// stream; always inspect `wait`, including after a consumer overload.
    pub async fn next_event(&self) -> Option<TaskEvent> {
        self.events.lock().await.recv().await
    }

    /// Authoritative outcome, independent of event consumption. Does not execute
    /// guest data or write artifacts to a host path. Cleanup failures retain a
    /// handle in `TaskFailure.cleanup`; success never hides pending cleanup.
    pub async fn wait(&self) -> Arc<TaskOutcome> {
        let mut receiver = self.outcome.clone();
        loop {
            if let Some(outcome) = receiver.borrow_and_update().clone() {
                return outcome;
            }
            if receiver.changed().await.is_err() {
                return Arc::new(Err(TaskFailure {
                    error: TaskError::WorkerGone,
                    cleanup: None,
                }));
            }
        }
    }
}

impl Drop for AgentTask {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub(crate) async fn guarded<T>(
    future: impl Future<Output = T>,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<T, TaskError> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(TaskError::Cancelled),
        _ = tokio::time::sleep_until(deadline) => Err(TaskError::TimedOut),
        result = future => Ok(result),
    }
}

fn sandbox_error(phase: TaskPhase, source: SandboxError) -> TaskError {
    TaskError::Sandbox {
        phase,
        source: Box::new(source),
    }
}

fn workspace_error(phase: TaskPhase, source: SandboxError) -> TaskError {
    TaskError::Workspace {
        phase,
        task: None,
        source: Box::new(source),
    }
}

fn with_task(error: TaskError, task: Option<TaskError>) -> TaskError {
    match error {
        TaskError::Workspace { phase, source, .. } => TaskError::Workspace {
            phase,
            task: task.map(Box::new),
            source,
        },
        other => other,
    }
}

/// Loads and restores the caller archive while the sandbox is still `Created`.
///
/// A missing archive is a first turn, not an error: only an archive path that
/// exists and fails validation is typed as a workspace failure. Input staging
/// runs afterwards in [`execute`], so caller inputs always win over archived
/// copies.
async fn restore_workspace(
    sandbox: &Sandbox,
    config: &RunnerConfig,
    request: &TaskRequest,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<(), TaskError> {
    let Some(workspace) = &request.workspace else {
        return Ok(());
    };
    let path = workspace.archive.clone();
    let budget = workspace.budget;
    let max_file_bytes = config.resources.max_file_bytes();
    let load = tokio::task::spawn_blocking(move || match std::fs::symlink_metadata(&path) {
        Ok(_) => CodeSnapshot::load_file(&path, max_file_bytes, budget).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(SnapshotError::from(error)),
    });
    let loaded = guarded(load, deadline, cancel)
        .await?
        .map_err(|_| TaskError::WorkerGone)?
        .map_err(|source| workspace_error(TaskPhase::Preparing, source.into()))?;
    if let Some(snapshot) = loaded {
        guarded(
            sandbox.restore_workspace_snapshot(Arc::new(snapshot)),
            deadline,
            cancel,
        )
        .await?
        .map_err(|source| workspace_error(TaskPhase::Preparing, source))?;
    }
    Ok(())
}

/// Persists the workspace before destruction. Cancellation and deadlines never
/// skip this attempt; only a sandbox that never reached `Running` has nothing
/// durable to capture. A capture/save failure is returned as a typed workspace
/// error while the caller still destroys the sandbox.
async fn persist_workspace(
    sandbox: &Sandbox,
    workspace: Option<&TaskWorkspace>,
    result: Result<TaskOutput, TaskError>,
) -> Result<TaskOutput, TaskError> {
    let Some(workspace) = workspace else {
        return result;
    };
    let status = match sandbox.status().await {
        Ok(status) => status,
        Err(source) => {
            return Err(with_task(
                workspace_error(TaskPhase::Exporting, source),
                result.err(),
            ));
        }
    };
    if status.phase != SandboxPhase::Running {
        return result;
    }
    let archive = workspace.archive.clone();
    let saved = match sandbox
        .capture_workspace_snapshot(workspace.budget, workspace.manifest_bytes())
        .await
    {
        Ok(snapshot) => tokio::task::spawn_blocking(move || snapshot.save_file(&archive))
            .await
            .map_err(|_| TaskError::WorkerGone)
            .and_then(|saved| {
                saved.map_err(|source| {
                    workspace_error(TaskPhase::Exporting, SandboxError::from(source))
                })
            }),
        Err(source) => Err(workspace_error(TaskPhase::Exporting, source)),
    };
    match saved {
        Ok(()) => result,
        Err(error) => Err(with_task(error, result.err())),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    id: &str,
    config: &RunnerConfig,
    request: TaskRequest,
    controller: &str,
    guest: &str,
    mcp_config: &str,
    cancel: &CancellationToken,
    events: &mut Events,
) -> TaskOutcome {
    let before_create = || -> Result<SandboxConfig, TaskError> {
        if cancel.is_cancelled() {
            return Err(TaskError::Cancelled);
        }
        SandboxConfig::new(
            config.resources,
            IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)
                .map_err(|_| TaskError::Invalid("isolation policy"))?,
            IoLimits::with_deadlines(
                PROCESS_BYTES,
                request.timeout,
                Duration::from_secs(15),
                Duration::from_secs(30),
            )
            .map_err(|_| TaskError::Invalid("I/O limits"))?,
        )
        .map_err(|_| TaskError::Invalid("sandbox configuration"))
    };
    let policy = before_create()
        .and_then(|policy| {
            events.send(TaskEvent::Phase(TaskPhase::Preparing))?;
            Ok(policy)
        })
        .map_err(|error| TaskFailure {
            error,
            cleanup: None,
        })?;
    let deadline = Instant::now() + request.timeout;
    // Do not abandon create: its returned receipt is needed if ownership remains
    // uncertain. A cancellation/deadline is observed as soon as creation settles.
    let sandbox = match Sandbox::create(config.backend.clone(), policy).await {
        Ok(sandbox) => Arc::new(sandbox),
        Err(source) => {
            let cleanup = match &source {
                SandboxError::CreationCleanupRequired { cleanup, .. } => {
                    Some(CleanupHandle(CleanupOwner::Creation(cleanup.clone())))
                }
                _ => None,
            };
            return Err(TaskFailure {
                error: sandbox_error(TaskPhase::Preparing, source),
                cleanup,
            });
        }
    };
    let workspace = request.workspace.clone();
    let mut process = None;
    let result = execute(
        id,
        &sandbox,
        config,
        request,
        controller,
        guest,
        mcp_config,
        deadline,
        cancel,
        events,
        &mut process,
    )
    .await;
    // A successful start is the only state in which a captured archive is
    // meaningful. Persist before destroy so a failed save still cleans up.
    let result = persist_workspace(&sandbox, workspace.as_ref(), result).await;
    // The guest session still owns cancellation until the archive attempt has
    // finished; dropping it here requests the same cleanup as before.
    drop(process);
    // Event backpressure may fail execution but must never suppress cleanup.
    let _ = events.send(TaskEvent::Phase(TaskPhase::Cleaning));
    match sandbox.destroy().await {
        Ok(()) => result.map_err(|error| TaskFailure {
            error,
            cleanup: None,
        }),
        Err(source) => Err(TaskFailure {
            error: TaskError::Cleanup {
                task: result.err().map(Box::new),
                source: Box::new(source),
            },
            cleanup: Some(CleanupHandle(CleanupOwner::Sandbox(sandbox))),
        }),
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    id: &str,
    sandbox: &Sandbox,
    config: &RunnerConfig,
    request: TaskRequest,
    controller: &str,
    guest: &str,
    mcp_config: &str,
    deadline: Instant,
    cancel: &CancellationToken,
    events: &mut Events,
    live: &mut Option<ProcessSession>,
) -> Result<TaskOutput, TaskError> {
    let prepare = async {
        let status = sandbox.status().await?;
        Ok::<_, SandboxError>(status.instance_id)
    };
    if let Some(instance_id) = guarded(prepare, deadline, cancel)
        .await?
        .map_err(|e| sandbox_error(TaskPhase::Preparing, e))?
    {
        events.send(TaskEvent::SandboxCreated { instance_id })?;
    }
    restore_workspace(sandbox, config, &request, deadline, cancel).await?;
    let staging = async {
        sandbox.create_dir_all("project").await?;
        sandbox.write_file("bridge.py", guest.as_bytes()).await?;
        if mcp_config != "[]" {
            sandbox
                .write_file("mcp_guest.py", MCP_GUEST.as_bytes())
                .await?;
        }
        for (path, data) in &request.input_files {
            let path = format!("project/{path}");
            if let Some((parent, _)) = path.rsplit_once('/') {
                sandbox.create_dir_all(parent).await?;
            }
            sandbox.write_file(&path, data).await?;
        }
        Ok::<_, SandboxError>(())
    };
    guarded(staging, deadline, cancel)
        .await?
        .map_err(|e| sandbox_error(TaskPhase::Preparing, e))?;
    guarded(sandbox.start(), deadline, cancel)
        .await?
        .map_err(|e| sandbox_error(TaskPhase::Preparing, e))?;
    let status = guarded(sandbox.status(), deadline, cancel)
        .await?
        .map_err(|e| sandbox_error(TaskPhase::Preparing, e))?;
    let policy = status.enforcement.ok_or(TaskError::Protocol)?;
    if policy.network_mode.as_deref() != Some("none")
        || policy.readonly_root != Some(true)
        || policy.bind_count != Some(0)
    {
        return Err(TaskError::Invalid("effective sandbox isolation"));
    }
    let token = uuid::Uuid::new_v4().to_string();
    let process = ProcessRequest {
        argv: vec![
            "circus-agent".into(),
            "python3".into(),
            "-I".into(),
            "-B".into(),
            "/workspace/bridge.py".into(),
        ],
        working_dir: Some("project".into()),
        env: BTreeMap::from([
            ("CIRCUS_TASK_TOKEN".into(), token.clone()),
            ("CIRCUS_MODEL".into(), config.model.model.clone()),
            (
                "CIRCUS_MCP_ENABLED".into(),
                if mcp_config == "[]" { "0" } else { "1" }.into(),
            ),
        ]),
        timeout: deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1)),
        output_limit_bytes: PROCESS_BYTES,
    };
    let session = guarded(sandbox.spawn_process(process), deadline, cancel)
        .await?
        .map_err(|e| sandbox_error(TaskPhase::Running, e))?;
    // Keep the session alive until the caller has attempted the archive; its
    // Drop requests lifecycle cleanup, which would otherwise close the window.
    *live = Some(session);
    let session = live.as_ref().ok_or(TaskError::WorkerGone)?;
    crate::process::exchange(
        session,
        config,
        &request.prompt,
        controller,
        mcp_config,
        &token,
        request.agent_session.as_deref(),
        deadline,
        cancel,
        events,
    )
    .await?;
    match guarded(session.wait(), deadline, cancel)
        .await?
        .map_err(|_| TaskError::Process)?
    {
        ProcessExit::Exited { exit_code: 0 } => {}
        _ => return Err(TaskError::Process),
    }
    events.send(TaskEvent::Phase(TaskPhase::Exporting))?;
    let mut artifacts = BTreeMap::new();
    let mut total = 0usize;
    for path in request.output_paths {
        let data = guarded(sandbox.read_file(project_path(&path)?), deadline, cancel)
            .await?
            .map_err(|e| sandbox_error(TaskPhase::Exporting, e))?;
        total = total
            .checked_add(data.len())
            .ok_or(TaskError::ArtifactLimit)?;
        if total > MAX_ARTIFACT_BYTES {
            return Err(TaskError::ArtifactLimit);
        }
        artifacts.insert(path, data);
    }
    Ok(TaskOutput {
        task_id: id.into(),
        artifacts,
        agent_session: events.agent_session().map(str::to_owned),
    })
}
