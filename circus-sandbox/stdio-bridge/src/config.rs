use std::path::{Path, PathBuf};

use crate::secrets::{KeychainRef, read_bounded, read_secret};
use circus_agent_runner::{AgentKind, AgentRunner, ModelConfig, RunnerConfig};
use circus_sandbox::{BackendConfig, ResourceLimits};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    version: u8,
    #[serde(default)]
    agent: Agent,
    docker_endpoint: String,
    agent_image: String,
    python: PathBuf,
    resources: Resources,
    model: Model,
    #[serde(default)]
    mcp_connections: Vec<crate::mcp_config::Connection>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Agent {
    #[default]
    Codex,
    ClaudeCode,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Resources {
    memory_bytes: u64,
    cpu_millis: u32,
    max_processes: u32,
    workspace_bytes: u64,
    max_file_bytes: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Model {
    base_url: String,
    model: String,
    api_key_file: Option<PathBuf>,
    api_key_keychain: Option<KeychainRef>,
    ca_file: Option<PathBuf>,
}

pub async fn load(path: &Path) -> Result<AgentRunner, ()> {
    let config: Config =
        sonic_rs::from_slice(&read_bounded(path, 64 * 1024).await?).map_err(|_| ())?;
    if config.version != 1 || !config.python.is_absolute() {
        return Err(());
    }
    let credential = String::from_utf8(
        read_secret(
            config.model.api_key_file.as_deref(),
            config.model.api_key_keychain.as_ref(),
            4098,
        )
        .await?,
    )
    .map_err(|_| ())?;
    let mut model = ModelConfig::new(
        config.model.base_url,
        config.model.model,
        credential.trim_end_matches(['\r', '\n']).into(),
    )
    .map_err(|_| ())?;
    if let Some(path) = config.model.ca_file {
        if !path.is_absolute() {
            return Err(());
        }
        model = model.with_ca_file(path);
    }
    let r = config.resources;
    let resources = ResourceLimits::new(
        r.memory_bytes,
        r.cpu_millis,
        r.max_processes,
        r.workspace_bytes,
        r.max_file_bytes,
    )
    .map_err(|_| ())?;
    let connections = crate::mcp_config::load(config.mcp_connections).await?;
    AgentRunner::new(RunnerConfig {
        backend: BackendConfig::docker(config.docker_endpoint, config.agent_image, "runc"),
        resources,
        model,
        python: config.python,
    })
    .and_then(|runner| runner.with_mcp_connections(connections))
    .map(|runner| {
        runner.with_agent(match config.agent {
            Agent::Codex => AgentKind::Codex,
            Agent::ClaudeCode => AgentKind::ClaudeCode,
        })
    })
    .map_err(|_| ())
}
