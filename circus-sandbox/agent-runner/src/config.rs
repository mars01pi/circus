use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use circus_sandbox::{BackendConfig, ResourceLimits, SnapshotBudget};

use crate::TaskError;

pub(crate) const MAX_FILES: usize = 64;
pub(crate) const MAX_INPUT: usize = 8 * 1024 * 1024;
pub(crate) const MAX_ARTIFACTS: usize = 32;
pub(crate) const MAX_ARTIFACT_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const PROCESS_BYTES: usize = 32 * 1024 * 1024;

/// Host-only model configuration. Debug output never contains credentials or URL.
#[derive(Clone)]
pub struct ModelConfig {
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) api_key: String,
    pub(crate) ca_file: Option<PathBuf>,
}

impl ModelConfig {
    pub fn new(base_url: String, model: String, api_key: String) -> Result<Self, TaskError> {
        let url = url::Url::parse(&base_url).map_err(|_| TaskError::Invalid("model HTTPS URL"))?;
        if url.scheme() != "https"
            || base_url.len() > 2048
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || base_url.contains('\\')
            || base_url.bytes().any(|b| b <= 32)
            || base_url.split('/').any(|part| part == "." || part == "..")
        {
            return Err(TaskError::Invalid("model HTTPS URL"));
        }
        if model.is_empty() || model.len() > 256 || model.chars().any(char::is_control) {
            return Err(TaskError::Invalid("model name"));
        }
        if api_key.is_empty()
            || api_key.len() > 4096
            || !api_key.bytes().all(|b| (32..=126).contains(&b))
        {
            return Err(TaskError::Invalid("model credential"));
        }
        Ok(Self {
            base_url,
            model,
            api_key,
            ca_file: None,
        })
    }

    /// Explicit trusted CA bundle for the host only. TLS verification stays on.
    pub fn with_ca_file(mut self, path: PathBuf) -> Self {
        self.ca_file = Some(path);
        self
    }
}

impl std::fmt::Debug for ModelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelConfig").finish_non_exhaustive()
    }
}

/// Trusted caller configuration; never populated from guest requests or ambient
/// Codex login files. Only the current Docker+runc backend is implemented.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub backend: BackendConfig,
    pub resources: ResourceLimits,
    pub model: ModelConfig,
    pub python: PathBuf,
}

/// Caller-owned durable workspace archive for sequential tasks.
///
/// The archive is a plaintext [`circus_sandbox::CodeSnapshot`] file on the host.
/// The first task may omit it (missing path means "start empty"); every task
/// with this value restores the archive into its `Created` sandbox before
/// staging the bridge and input files, and captures the workspace back after the
/// turn. The sandbox itself is recreated per task: this is durable files, not an
/// in-flight process or turn checkpoint.
#[derive(Clone)]
pub struct TaskWorkspace {
    /// Host path owned by the caller. Never passed to the guest or logged.
    pub archive: PathBuf,
    /// Bounds for both archive load and workspace capture.
    pub budget: SnapshotBudget,
}

impl std::fmt::Debug for TaskWorkspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskWorkspace")
            .field("archive", &"[caller-owned host path]")
            .field("budget", &self.budget)
            .finish()
    }
}

impl TaskWorkspace {
    /// Manifest metadata allowance derived from the caller's entry budget.
    /// Every entry has a bounded path plus record headers.
    pub(crate) fn manifest_bytes(&self) -> u64 {
        const MAX_PATH_BYTES: u64 = 4096;
        const RECORD_OVERHEAD: u64 = 8;
        u64::try_from(self.budget.max_entries())
            .unwrap_or(u64::MAX)
            .saturating_mul(MAX_PATH_BYTES + RECORD_OVERHEAD)
            .saturating_add(64)
    }
}

/// One task over a fresh sandbox. Paths are relative to `/workspace/project`.
/// Inputs and artifacts are bounded bytes, not grants to host filesystem paths.
pub struct TaskRequest {
    pub prompt: String,
    pub input_files: BTreeMap<String, Vec<u8>>,
    pub output_paths: Vec<String>,
    /// Execution deadline; owned creation and cleanup must settle even after it.
    pub timeout: Duration,
    /// Durable workspace for sequential turns. Each task still owns a new sandbox.
    pub workspace: Option<TaskWorkspace>,
    /// Native agent thread to resume. Codex only; Claude Code fails closed.
    pub agent_session: Option<String>,
}

impl TaskRequest {
    pub(crate) fn validate(&self, max_file: usize) -> Result<(), TaskError> {
        if self.prompt.is_empty() || self.prompt.len() > 64 * 1024 || self.prompt.contains('\0') {
            return Err(TaskError::Invalid(
                "prompt must contain 1..65536 bytes without NUL",
            ));
        }
        if self.timeout.is_zero() || self.timeout > Duration::from_secs(3600) {
            return Err(TaskError::Invalid("task timeout must be in (0, 1 hour]"));
        }
        if self.input_files.len() > MAX_FILES || self.output_paths.len() > MAX_ARTIFACTS {
            return Err(TaskError::Invalid("too many input files or artifact paths"));
        }
        let mut total = 0usize;
        for (path, data) in &self.input_files {
            project_path(path)?;
            if data.len() > max_file {
                return Err(TaskError::Invalid("input file exceeds per-file limit"));
            }
            total = total
                .checked_add(data.len())
                .ok_or(TaskError::Invalid("input size overflow"))?;
            if total > MAX_INPUT {
                return Err(TaskError::Invalid("input byte budget exceeded"));
            }
        }
        let mut paths = std::collections::BTreeSet::new();
        for path in &self.output_paths {
            project_path(path)?;
            if !paths.insert(path) {
                return Err(TaskError::Invalid("duplicate artifact path"));
            }
        }
        if let Some(workspace) = &self.workspace
            && (workspace.archive.as_os_str().is_empty()
                || workspace
                    .archive
                    .as_os_str()
                    .to_string_lossy()
                    .chars()
                    .any(char::is_control))
        {
            return Err(TaskError::Invalid("workspace archive path"));
        }
        if let Some(session) = &self.agent_session
            && !valid_session_id(session)
        {
            return Err(TaskError::Invalid("agent session id"));
        }
        Ok(())
    }
}

/// Codex thread ids are opaque but bounded; the bridge applies the same rule.
pub(crate) fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(crate) fn project_path(path: &str) -> Result<String, TaskError> {
    if path.len() > 1024
        || path.contains(['\\', ':'])
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(TaskError::Invalid("invalid project-relative path"));
    }
    Ok(format!("project/{path}"))
}
