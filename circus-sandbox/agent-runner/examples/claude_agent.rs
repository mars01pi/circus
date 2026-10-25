//! Explicit local example. The library itself never reads ambient credentials.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use circus_agent_runner::{
    AgentKind, AgentRunner, ModelConfig, RunnerConfig, TaskEvent, TaskRequest,
};
use circus_sandbox::{BackendConfig, ResourceLimits};

fn required(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    std::env::var(name).map_err(|_| format!("set {name} explicitly").into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut model = ModelConfig::new(
        required("CIRCUS_MODEL_BASE_URL")?,
        required("CIRCUS_MODEL")?,
        required("CIRCUS_MODEL_API_KEY")?,
    )?;
    if let Some(path) = std::env::var_os("CIRCUS_MODEL_CA_FILE") {
        model = model.with_ca_file(path.into());
    }
    let runner = AgentRunner::new(RunnerConfig {
        backend: BackendConfig::docker(
            required("CIRCUS_DOCKER_ENDPOINT")?,
            required("CIRCUS_AGENT_IMAGE")?,
            "runc",
        ),
        resources: ResourceLimits::new(
            512 * 1024 * 1024,
            1000,
            128,
            128 * 1024 * 1024,
            2 * 1024 * 1024,
        )?,
        model,
        python: std::env::var_os("CIRCUS_HOST_PYTHON")
            .map(PathBuf::from)
            .unwrap_or_else(|| "python3".into()),
    })?
    .with_agent(AgentKind::ClaudeCode);
    let task = runner.start(TaskRequest {
        prompt: required("CIRCUS_AGENT_PROMPT")?,
        input_files: BTreeMap::from([(
            "answer.py".into(),
            b"def add(a, b): return a - b\n".to_vec(),
        )]),
        output_paths: vec!["answer.py".into(), "result.txt".into()],
        timeout: Duration::from_secs(180),
        workspace: None,
        agent_session: None,
    })?;
    while let Some(event) = task.next_event().await {
        // This example prints only trusted state and static event categories.
        // A UI can render text/tool payloads as untrusted text, without executing it.
        match event {
            TaskEvent::Phase(phase) => eprintln!("task phase: {phase:?}"),
            TaskEvent::SandboxCreated { instance_id } => eprintln!("task sandbox: {instance_id}"),
            TaskEvent::Tool { phase, .. } => eprintln!("tool: {phase:?}"),
            _ => {}
        }
    }
    let outcome = task.wait().await;
    match outcome.as_ref() {
        Ok(result) => {
            println!(
                "task completed; {} artifacts retrieved",
                result.artifacts.len()
            );
            for (path, bytes) in &result.artifacts {
                println!("{path}: {} bytes", bytes.len());
            }
            if let Some(bytes) = result.artifacts.get("result.txt") {
                use std::io::Write;
                std::io::stdout().lock().write_all(bytes)?;
            }
            Ok(())
        }
        Err(failure) => {
            Err(format!("{failure}; cleanup pending: {}", failure.cleanup.is_some()).into())
        }
    }
}
