//! Strict JSON configuration and value-model validation.
//!
//! Every security-relevant value is explicit. There are no host defaults,
//! inherited environment variables, or implicit credentials. Validation here
//! runs before any backend connection.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use circus_sandbox::environment::{ImagePlatform, ImagePullPolicy};
use circus_sandbox::{
    ConfigError as SandboxConfigError, IoLimits, IsolationConfig, ProcessRequest, ResourceLimits,
    SandboxConfig, SnapshotBudget,
};
use serde::Deserialize;

const MAX_CONFIG_BYTES: usize = 256 * 1024;
const MANIFEST_ENTRY_ALLOWANCE: u64 = 1024;
const MANIFEST_FIXED_ALLOWANCE: u64 = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ConfigLoadError {
    #[error("configuration file is not a regular file")]
    NotFile,
    #[error("configuration file exceeds its size limit")]
    TooLarge,
    #[error("configuration file could not be read")]
    Read,
    #[error("configuration is not valid JSON")]
    Json,
    #[error("configuration version must be 1")]
    Version,
    #[error("configuration field {field} is invalid")]
    Field { field: &'static str },
    #[error("configuration field {field} must be positive")]
    Zero { field: &'static str },
    #[error("configuration field {field} must be an absolute path")]
    Absolute { field: &'static str },
    #[error("archive and state paths must differ")]
    SamePath,
    #[error("imagePlatform is required for Docker and containerd backends")]
    PlatformRequired,
    #[error("imagePrepareTimeoutMs is required for Docker and containerd backends")]
    PrepareTimeoutRequired,
    #[error("the parent directory for configuration field {field} is missing")]
    MissingParent { field: &'static str },
    #[error("resource limits were rejected: {0}")]
    Limits(#[from] circus_sandbox::ResourceLimitsError),
    #[error(transparent)]
    Sandbox(#[from] SandboxConfigError),
}

/// Backend descriptor as data. It is converted to the public descriptor only
/// after optional image preparation resolves an immutable image identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendPlan {
    Docker {
        endpoint: String,
        image: String,
        runtime: String,
    },
    Containerd {
        socket: String,
        namespace: String,
        image: String,
        runtime: String,
        snapshotter: String,
    },
    Kubernetes {
        kubeconfig: String,
        context: Option<String>,
        namespace: String,
        image: String,
        service_account: Option<String>,
    },
}

impl BackendPlan {
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Docker { .. } => "docker",
            Self::Containerd { .. } => "containerd",
            Self::Kubernetes { .. } => "kubernetes",
        }
    }

    #[must_use]
    pub fn uses_local_image(&self) -> bool {
        !matches!(self, Self::Kubernetes { .. })
    }

    /// Original descriptor, used for orphan recovery and as the base for image
    /// preparation. Recovery never prepares or pulls.
    #[must_use]
    pub fn descriptor(&self) -> circus_sandbox::BackendConfig {
        match self {
            Self::Docker {
                endpoint,
                image,
                runtime,
            } => circus_sandbox::BackendConfig::docker(
                endpoint.as_str(),
                image.as_str(),
                runtime.as_str(),
            ),
            Self::Containerd {
                socket,
                namespace,
                image,
                runtime,
                snapshotter,
            } => circus_sandbox::BackendConfig::containerd(
                socket.as_str(),
                namespace.as_str(),
                image.as_str(),
                runtime.as_str(),
                snapshotter.as_str(),
            ),
            Self::Kubernetes {
                kubeconfig,
                context,
                namespace,
                image,
                service_account,
            } => {
                let mut descriptor = circus_sandbox::KubernetesBackendConfig::new(
                    kubeconfig.as_str(),
                    namespace.as_str(),
                    image.as_str(),
                );
                if let Some(context) = context {
                    descriptor = descriptor.with_context(context.as_str());
                }
                if let Some(service_account) = service_account {
                    descriptor = descriptor.with_service_account(service_account.as_str());
                }
                circus_sandbox::BackendConfig::Kubernetes(descriptor)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImageSettings {
    pub platform: Option<ImagePlatform>,
    pub prepare_timeout: Option<Duration>,
    pub pull_policy: ImagePullPolicy,
}

/// Explicit guest process values. Host environment variables are never copied.
#[derive(Debug, Clone)]
pub struct ProcessTemplate {
    pub working_dir: Option<String>,
    pub env: BTreeMap<String, String>,
    pub timeout: Duration,
    pub output_limit_bytes: usize,
}

impl ProcessTemplate {
    #[must_use]
    pub fn request(&self, argv: Vec<String>) -> ProcessRequest {
        ProcessRequest {
            argv,
            working_dir: self.working_dir.clone(),
            env: self.env.clone(),
            timeout: self.timeout,
            output_limit_bytes: self.output_limit_bytes,
        }
    }
}

/// Fully validated runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub backend: BackendPlan,
    pub image: ImageSettings,
    pub sandbox: SandboxConfig,
    pub budget: SnapshotBudget,
    pub archive_path: PathBuf,
    /// Transport bound for the workspace manifest, derived from the payload
    /// and entry budgets with a fixed, bounded per-entry allowance.
    pub archive_max_bytes: u64,
    pub state_path: PathBuf,
    pub process: ProcessTemplate,
}

impl Config {
    /// Reads and validates a trusted caller-owned configuration file.
    pub fn load(path: &Path) -> Result<Self, ConfigLoadError> {
        let metadata = std::fs::metadata(path).map_err(|_| ConfigLoadError::Read)?;
        if !metadata.is_file() {
            return Err(ConfigLoadError::NotFile);
        }
        if metadata.len() > MAX_CONFIG_BYTES as u64 {
            return Err(ConfigLoadError::TooLarge);
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        std::fs::File::open(path)
            .and_then(|file| {
                file.take(MAX_CONFIG_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
            })
            .map_err(|_| ConfigLoadError::Read)?;
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ConfigLoadError::TooLarge);
        }
        Self::parse(&bytes)
    }

    /// Validates JSON bytes without touching the filesystem.
    pub fn parse(bytes: &[u8]) -> Result<Self, ConfigLoadError> {
        let raw: RawConfig = sonic_rs::from_slice(bytes).map_err(|_| ConfigLoadError::Json)?;
        if raw.version != 1 {
            return Err(ConfigLoadError::Version);
        }

        let limits = ResourceLimits::new(
            raw.limits.memory_bytes,
            raw.limits.cpu_millis,
            raw.limits.max_processes,
            raw.limits.workspace_bytes,
            raw.limits.max_file_bytes,
        )?;
        let requested_pids = limits.pids();
        let requested_uid = raw.isolation.guest_uid;
        let isolation = IsolationConfig::linux_enforced(
            true,
            true,
            true,
            true,
            raw.isolation.workspace_path,
            raw.isolation.guest_uid,
            raw.isolation.guest_gid,
        )?;
        let io = IoLimits::with_deadlines(
            positive_usize(raw.io.output_limit_bytes, "io.outputLimitBytes")?,
            millis(raw.io.command_timeout_ms, "io.commandTimeoutMs")?,
            millis(raw.io.file_timeout_ms, "io.fileTimeoutMs")?,
            millis(raw.io.lifecycle_timeout_ms, "io.lifecycleTimeoutMs")?,
        )
        .map_err(map_sandbox_config)?;
        let sandbox = SandboxConfig::new(limits, isolation, io).map_err(map_sandbox_config)?;

        let budget = SnapshotBudget::new(
            positive_usize(raw.workspace.max_total_bytes, "workspace.maxTotalBytes")?,
            positive_usize(raw.workspace.max_entries, "workspace.maxEntries")?,
        )
        .map_err(|_| ConfigLoadError::Field {
            field: "workspace budgets",
        })?;

        let archive_path = absolute(raw.workspace.archive_path, "workspace.archivePath")?;
        let state_path = absolute(raw.state_path, "statePath")?;
        if archive_path == state_path {
            return Err(ConfigLoadError::SamePath);
        }

        let backend = raw.backend.into_plan()?;
        if matches!(backend, BackendPlan::Docker { .. }) && requested_pids < 3 {
            return Err(ConfigLoadError::Field {
                field: "limits.maxProcesses",
            });
        }
        if matches!(backend, BackendPlan::Kubernetes { .. }) && requested_uid == 0 {
            return Err(ConfigLoadError::Field {
                field: "isolation.guestUid",
            });
        }
        let platform = match raw.image_platform {
            Some(platform) => Some(
                ImagePlatform::with_variant(platform.os, platform.architecture, platform.variant)
                    .map_err(|_| ConfigLoadError::Field {
                    field: "imagePlatform",
                })?,
            ),
            None => None,
        };
        if backend.uses_local_image() && platform.is_none() {
            return Err(ConfigLoadError::PlatformRequired);
        }
        let prepare_timeout = raw
            .image_prepare_timeout_ms
            .map(|value| millis(value, "imagePrepareTimeoutMs"))
            .transpose()?;
        if backend.uses_local_image() && prepare_timeout.is_none() {
            return Err(ConfigLoadError::PrepareTimeoutRequired);
        }
        let pull_policy = match raw.image_pull_policy {
            PullPolicy::LocalOnly => ImagePullPolicy::LocalOnly,
            PullPolicy::IfMissing => ImagePullPolicy::IfMissing,
        };

        let process = ProcessTemplate {
            working_dir: raw.agent.working_dir,
            env: raw.agent.env,
            timeout: millis(raw.agent.timeout_ms, "agent.timeoutMs")?,
            output_limit_bytes: positive_usize(
                raw.agent.output_limit_bytes,
                "agent.outputLimitBytes",
            )?,
        };
        validate_process(&process)?;

        let entries = budget.max_entries() as u64;
        let archive_max_bytes = (budget.max_total_bytes() as u64)
            .saturating_add(entries.saturating_mul(MANIFEST_ENTRY_ALLOWANCE))
            .saturating_add(MANIFEST_FIXED_ALLOWANCE);

        Ok(Self {
            backend,
            image: ImageSettings {
                platform,
                prepare_timeout,
                pull_policy,
            },
            sandbox,
            budget,
            archive_path,
            archive_max_bytes,
            state_path,
            process,
        })
    }

    /// Rejects argv that the sandbox API would reject after acquiring resources.
    pub fn validate_argv(&self, argv: &[String]) -> Result<(), ConfigLoadError> {
        if argv.is_empty()
            || argv.first().is_none_or(String::is_empty)
            || argv.iter().any(|argument| argument.contains('\0'))
        {
            return Err(ConfigLoadError::Field {
                field: "guest argv",
            });
        }
        Ok(())
    }

    /// Requires both caller-owned destination parents to exist before a backend
    /// is contacted. The files themselves may or may not exist.
    pub fn check_destinations(&self) -> Result<(), ConfigLoadError> {
        for (field, path) in [
            ("workspace.archivePath", &self.archive_path),
            ("statePath", &self.state_path),
        ] {
            let parent = path
                .parent()
                .ok_or(ConfigLoadError::MissingParent { field })?;
            if !parent.is_dir() {
                return Err(ConfigLoadError::MissingParent { field });
            }
        }
        Ok(())
    }
}

fn validate_process(process: &ProcessTemplate) -> Result<(), ConfigLoadError> {
    if let Some(working_dir) = &process.working_dir
        && (!valid_relative(working_dir) || working_dir.contains('\0'))
    {
        return Err(ConfigLoadError::Field {
            field: "agent.workingDir",
        });
    }
    for (name, value) in &process.env {
        let mut bytes = name.bytes();
        if !bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || value.contains('\0')
        {
            return Err(ConfigLoadError::Field { field: "agent.env" });
        }
    }
    if Instant::now().checked_add(process.timeout).is_none() {
        return Err(ConfigLoadError::Field {
            field: "agent.timeoutMs",
        });
    }
    Ok(())
}

/// Mirrors the sandbox API's relative-path rule for guest working directories.
fn valid_relative(value: &str) -> bool {
    if value.is_empty() || value.contains('\\') || value.starts_with('/') {
        return false;
    }
    value.split('/').all(|component| {
        !component.is_empty()
            && component != "."
            && component != ".."
            && !(component.len() >= 2
                && component.as_bytes()[0].is_ascii_alphabetic()
                && component.as_bytes()[1] == b':')
    })
}

fn positive(value: u64, field: &'static str) -> Result<u64, ConfigLoadError> {
    if value == 0 {
        Err(ConfigLoadError::Zero { field })
    } else {
        Ok(value)
    }
}

fn positive_usize(value: usize, field: &'static str) -> Result<usize, ConfigLoadError> {
    if value == 0 {
        Err(ConfigLoadError::Zero { field })
    } else if value == usize::MAX {
        Err(ConfigLoadError::Field { field })
    } else {
        Ok(value)
    }
}

fn millis(value: u64, field: &'static str) -> Result<Duration, ConfigLoadError> {
    positive(value, field).map(Duration::from_millis)
}

fn absolute(value: String, field: &'static str) -> Result<PathBuf, ConfigLoadError> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(ConfigLoadError::Absolute { field })
    }
}

fn map_sandbox_config(error: SandboxConfigError) -> ConfigLoadError {
    match error {
        SandboxConfigError::InvalidOutputLimit { .. } => ConfigLoadError::Field {
            field: "io.outputLimitBytes",
        },
        SandboxConfigError::InvalidFileTimeout => ConfigLoadError::Field {
            field: "io.fileTimeoutMs",
        },
        SandboxConfigError::InvalidCommandTimeout => ConfigLoadError::Field {
            field: "io.commandTimeoutMs",
        },
        SandboxConfigError::InvalidLifecycleTimeout => ConfigLoadError::Field {
            field: "io.lifecycleTimeoutMs",
        },
        other => ConfigLoadError::Sandbox(other),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawConfig {
    version: u8,
    backend: RawBackend,
    image_pull_policy: PullPolicy,
    #[serde(default)]
    image_platform: Option<RawPlatform>,
    #[serde(default)]
    image_prepare_timeout_ms: Option<u64>,
    limits: RawLimits,
    isolation: RawIsolation,
    io: RawIo,
    workspace: RawWorkspace,
    state_path: String,
    agent: RawAgent,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawBackend {
    kind: String,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    runtime: Option<String>,
    #[serde(default)]
    socket: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    snapshotter: Option<String>,
    #[serde(default)]
    kubeconfig: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    service_account: Option<String>,
}

impl RawBackend {
    fn into_plan(self) -> Result<BackendPlan, ConfigLoadError> {
        let kind = self.kind;
        match kind.as_str() {
            "docker" => {
                if self.socket.is_some()
                    || self.namespace.is_some()
                    || self.snapshotter.is_some()
                    || self.kubeconfig.is_some()
                    || self.context.is_some()
                    || self.service_account.is_some()
                {
                    return Err(ConfigLoadError::Field {
                        field: "backend.kind",
                    });
                }
                let endpoint = required(self.endpoint, "backend.endpoint")?;
                let image = required(self.image, "backend.image")?;
                let runtime = required(self.runtime, "backend.runtime")?;
                Ok(BackendPlan::Docker {
                    endpoint,
                    image,
                    runtime,
                })
            }
            "containerd" => {
                if self.endpoint.is_some()
                    || self.kubeconfig.is_some()
                    || self.context.is_some()
                    || self.service_account.is_some()
                {
                    return Err(ConfigLoadError::Field {
                        field: "backend.kind",
                    });
                }
                let socket = required(self.socket, "backend.socket")?;
                let namespace = required(self.namespace, "backend.namespace")?;
                let image = required(self.image, "backend.image")?;
                let runtime = required(self.runtime, "backend.runtime")?;
                let snapshotter = required(self.snapshotter, "backend.snapshotter")?;
                Ok(BackendPlan::Containerd {
                    socket,
                    namespace,
                    image,
                    runtime,
                    snapshotter,
                })
            }
            "kubernetes" => {
                if self.endpoint.is_some()
                    || self.runtime.is_some()
                    || self.socket.is_some()
                    || self.snapshotter.is_some()
                {
                    return Err(ConfigLoadError::Field {
                        field: "backend.kind",
                    });
                }
                let kubeconfig = required(self.kubeconfig, "backend.kubeconfig")?;
                let namespace = required(self.namespace, "backend.namespace")?;
                let image = required(self.image, "backend.image")?;
                Ok(BackendPlan::Kubernetes {
                    kubeconfig,
                    context: self.context,
                    namespace,
                    image,
                    service_account: self.service_account,
                })
            }
            _ => Err(ConfigLoadError::Field {
                field: "backend.kind",
            }),
        }
    }
}

fn required<T>(value: Option<T>, field: &'static str) -> Result<T, ConfigLoadError> {
    value.ok_or(ConfigLoadError::Field { field })
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum PullPolicy {
    LocalOnly,
    IfMissing,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawPlatform {
    os: String,
    architecture: String,
    #[serde(default)]
    variant: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawLimits {
    memory_bytes: u64,
    cpu_millis: u32,
    max_processes: u32,
    workspace_bytes: u64,
    max_file_bytes: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawIsolation {
    workspace_path: String,
    guest_uid: u32,
    guest_gid: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawIo {
    output_limit_bytes: usize,
    command_timeout_ms: u64,
    file_timeout_ms: u64,
    lifecycle_timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawWorkspace {
    archive_path: String,
    max_total_bytes: usize,
    max_entries: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawAgent {
    working_dir: Option<String>,
    env: BTreeMap<String, String>,
    timeout_ms: u64,
    output_limit_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn config_bytes(dir: &TempDir) -> Vec<u8> {
        let root = dir.path().to_string_lossy().replace('\\', "\\\\");
        format!(
            r#"{{
                "version": 1,
                "backend": {{"kind": "docker", "endpoint": "unix:///var/run/docker.sock", "image": "python:3.12-slim", "runtime": "runc"}},
                "imagePullPolicy": "local-only",
                "imagePlatform": {{"os": "linux", "architecture": "amd64"}},
                "imagePrepareTimeoutMs": 60000,
                "limits": {{"memoryBytes": 536870912, "cpuMillis": 1000, "maxProcesses": 64, "workspaceBytes": 16777216, "maxFileBytes": 1048576}},
                "isolation": {{"workspacePath": "/workspace", "guestUid": 1000, "guestGid": 1000}},
                "io": {{"outputLimitBytes": 1048576, "commandTimeoutMs": 30000, "fileTimeoutMs": 30000, "lifecycleTimeoutMs": 30000}},
                "workspace": {{"archivePath": "{root}/workspace.snap", "maxTotalBytes": 16777216, "maxEntries": 1024}},
                "statePath": "{root}/state.json",
                "agent": {{"workingDir": "project", "env": {{"MODE": "test"}}, "timeoutMs": 60000, "outputLimitBytes": 1048576}}
            }}"#
        )
        .into_bytes()
    }

    #[test]
    fn parses_and_exposes_explicit_values() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::parse(&config_bytes(&dir)).unwrap();
        assert_eq!(config.backend.kind(), "docker");
        assert_eq!(config.sandbox.limits().memory_bytes(), 536_870_912);
        assert_eq!(config.sandbox.isolation().workspace_path(), "/workspace");
        assert_eq!(
            config.sandbox.command_timeout(),
            Duration::from_millis(30_000)
        );
        assert_eq!(config.budget.max_entries(), 1024);
        assert!(config.archive_max_bytes > config.budget.max_total_bytes() as u64);
        assert_eq!(config.process.working_dir.as_deref(), Some("project"));
        assert_eq!(
            config.process.env.get("MODE").map(String::as_str),
            Some("test")
        );
        assert_eq!(config.process.timeout, Duration::from_millis(60_000));
        assert_eq!(config.image.pull_policy, ImagePullPolicy::LocalOnly);
        assert!(config.image.platform.is_some());
        config.check_destinations().unwrap();
    }

    #[test]
    fn rejects_unknown_fields_at_every_level() {
        let dir = tempfile::tempdir().unwrap();
        let mut text = String::from_utf8(config_bytes(&dir)).unwrap();
        text = text.replace("\"version\": 1,", "\"version\": 1, \"extra\": 1,");
        assert!(matches!(
            Config::parse(text.as_bytes()),
            Err(ConfigLoadError::Json)
        ));
        let mut text = String::from_utf8(config_bytes(&dir)).unwrap();
        text = text.replace("\"guestUid\": 1000", "\"guestUid\": 1000, \"flag\": true");
        assert!(matches!(
            Config::parse(text.as_bytes()),
            Err(ConfigLoadError::Json)
        ));
        let mut text = String::from_utf8(config_bytes(&dir)).unwrap();
        text = text.replace(
            "\"runtime\": \"runc\"",
            "\"runtime\": \"runc\", \"extra\": 1",
        );
        assert!(matches!(
            Config::parse(text.as_bytes()),
            Err(ConfigLoadError::Json)
        ));
        let text = String::from_utf8(config_bytes(&dir)).unwrap();
        let text = text.replace(
            "\"runtime\": \"runc\"",
            "\"runtime\": \"runc\", \"namespace\": \"other\"",
        );
        assert!(matches!(
            Config::parse(text.as_bytes()),
            Err(ConfigLoadError::Field { .. })
        ));
    }

    #[test]
    fn rejects_wrong_version_and_insecure_values() {
        let dir = tempfile::tempdir().unwrap();
        let text = String::from_utf8(config_bytes(&dir)).unwrap();
        let wrong = text.replace("\"version\": 1", "\"version\": 2");
        assert!(matches!(
            Config::parse(wrong.as_bytes()),
            Err(ConfigLoadError::Version)
        ));
        let zero_pids = text.replace("\"maxProcesses\": 64", "\"maxProcesses\": 0");
        assert!(matches!(
            Config::parse(zero_pids.as_bytes()),
            Err(ConfigLoadError::Limits(_))
        ));
        let relative = text.replace(
            "\"workspacePath\": \"/workspace\"",
            "\"workspacePath\": \"workspace\"",
        );
        assert!(Config::parse(relative.as_bytes()).is_err());
        let zero_output = text.replace(
            "\"outputLimitBytes\": 1048576, \"commandTimeoutMs\"",
            "\"outputLimitBytes\": 0, \"commandTimeoutMs\"",
        );
        assert!(matches!(
            Config::parse(zero_output.as_bytes()),
            Err(ConfigLoadError::Zero { .. })
        ));
    }

    #[test]
    fn requires_image_settings_only_for_local_backends() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().replace('\\', "\\\\");
        let text = String::from_utf8(config_bytes(&dir)).unwrap();
        let without = text
            .replace(
                "\"imagePlatform\": {\"os\": \"linux\", \"architecture\": \"amd64\"},",
                "",
            )
            .replace("\"imagePrepareTimeoutMs\": 60000,", "");
        assert!(matches!(
            Config::parse(without.as_bytes()),
            Err(ConfigLoadError::PlatformRequired)
        ));

        let kubernetes = format!(
            r#"{{
                "version": 1,
                "backend": {{"kind": "kubernetes", "kubeconfig": "{root}/kubeconfig", "namespace": "sandboxes", "image": "python:3.12-slim"}},
                "imagePullPolicy": "if-missing",
                "limits": {{"memoryBytes": 536870912, "cpuMillis": 1000, "maxProcesses": 64, "workspaceBytes": 16777216, "maxFileBytes": 1048576}},
                "isolation": {{"workspacePath": "/workspace", "guestUid": 1000, "guestGid": 1000}},
                "io": {{"outputLimitBytes": 1048576, "commandTimeoutMs": 30000, "fileTimeoutMs": 30000, "lifecycleTimeoutMs": 30000}},
                "workspace": {{"archivePath": "{root}/workspace.snap", "maxTotalBytes": 16777216, "maxEntries": 1024}},
                "statePath": "{root}/state.json",
                "agent": {{"workingDir": null, "env": {{}}, "timeoutMs": 60000, "outputLimitBytes": 1048576}}
            }}"#
        );
        let config = Config::parse(kubernetes.as_bytes()).unwrap();
        assert_eq!(config.backend.kind(), "kubernetes");
        assert!(config.image.platform.is_none());
    }

    #[test]
    fn rejects_unsafe_paths_argv_and_environment() {
        let dir = tempfile::tempdir().unwrap();
        let base = String::from_utf8(config_bytes(&dir)).unwrap();
        let config = Config::parse(base.as_bytes()).unwrap();
        for argv in [
            vec![],
            vec![String::new()],
            vec!["codex".to_string(), "bad\0arg".to_string()],
        ] {
            assert!(config.validate_argv(&argv).is_err());
        }
        config
            .validate_argv(&["codex".to_string(), "app-server".to_string()])
            .unwrap();

        for (from, to) in [
            (
                "\"workingDir\": \"project\"",
                "\"workingDir\": \"../escape\"",
            ),
            (
                "\"workingDir\": \"project\"",
                "\"workingDir\": \"/absolute\"",
            ),
            ("\"workingDir\": \"project\"", "\"workingDir\": \"a\\\\b\""),
            ("\"MODE\": \"test\"", "\"1BAD\": \"test\""),
            ("\"MODE\": \"test\"", "\"MODE\": \"bad\\u0000value\""),
        ] {
            let text = base.replace(from, to);
            assert!(
                Config::parse(text.as_bytes()).is_err(),
                "expected rejection for {to}"
            );
        }

        let relative_archive = base.replace(
            format!(
                "\"archivePath\": \"{}/workspace.snap\"",
                dir.path().to_string_lossy().replace('\\', "\\\\")
            )
            .as_str(),
            "\"archivePath\": \"workspace.snap\"",
        );
        assert!(matches!(
            Config::parse(relative_archive.as_bytes()),
            Err(ConfigLoadError::Absolute { .. })
        ));
    }

    #[test]
    fn request_forwards_only_configured_values() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::parse(&config_bytes(&dir)).unwrap();
        let request = config
            .process
            .request(vec!["codex".to_string(), "app-server".to_string()]);
        assert_eq!(request.argv, ["codex", "app-server"]);
        assert_eq!(request.working_dir.as_deref(), Some("project"));
        assert_eq!(request.env.len(), 1);
        assert!(!request.env.contains_key("PATH"));
        assert_eq!(request.timeout, Duration::from_millis(60_000));
        assert_eq!(request.output_limit_bytes, 1_048_576);
    }
}
