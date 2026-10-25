use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use circus_sandbox::{BackendConfig, ResourceLimits};

use crate::{AgentKind, AgentRunner, ModelConfig, RunnerConfig, TaskError, TaskEvent, TaskRequest};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

#[tokio::test]
#[ignore = "requires explicit real Linux Docker and pinned Claude Code image"]
async fn claude_cancel_and_timeout_revoke_requests_and_confirm_container_cleanup() -> Result {
    assert_eq!(std::env::var("CIRCUS_DOCKER_SANDBOX").as_deref(), Ok("1"));
    let endpoint = std::env::var("CIRCUS_DOCKER_ENDPOINT")?;
    let runner = AgentRunner::new(RunnerConfig {
        backend: BackendConfig::docker(
            endpoint.clone(),
            std::env::var("CIRCUS_CLAUDE_IMAGE")?,
            "runc",
        ),
        resources: ResourceLimits::new(
            512 * 1024 * 1024,
            1000,
            128,
            128 * 1024 * 1024,
            2 * 1024 * 1024,
        )?,
        model: ModelConfig::new(
            "https://unused.invalid/v1".into(),
            "circus-fixture".into(),
            "host-only-claude-sentinel".into(),
        )?,
        python: std::env::var_os("CIRCUS_HOST_PYTHON")
            .unwrap_or_else(|| "python3".into())
            .into(),
    })?
    .with_agent(AgentKind::ClaudeCode);
    for cancel in [true, false] {
        let request = TaskRequest {
            prompt: "Say hello.".into(),
            input_files: BTreeMap::new(),
            output_paths: vec![],
            timeout: Duration::from_secs(if cancel { 90 } else { 30 }),
            workspace: None,
            agent_session: None,
        };
        let controller = format!(
            "{}\nclass HangingMessages:\n def forward(self, body, emit):\n  import time\n  event('text', text='fixture request reached')\n  time.sleep(300)\nmain_claude_host(HangingMessages())\n",
            AgentKind::ClaudeCode.source()
        );
        let task = runner.start_controller(request, controller)?;
        let mut instance = None;
        let mut reached = false;
        tokio::time::timeout(Duration::from_secs(100), async {
            while let Some(event) = task.next_event().await {
                match event {
                    TaskEvent::SandboxCreated { instance_id } => instance = Some(instance_id),
                    TaskEvent::TextDelta { text } if text == "fixture request reached" => {
                        reached = true;
                        if cancel {
                            task.cancel();
                        }
                    }
                    _ => {}
                }
            }
        })
        .await?;
        let outcome = task.wait().await;
        let failure = outcome.as_ref().as_ref().unwrap_err();
        assert!(
            reached,
            "real Claude CLI did not request Messages: {failure:?}"
        );
        assert!(
            matches!(
                (&failure.error, cancel),
                (TaskError::Cancelled, true) | (TaskError::TimedOut, false)
            ),
            "{failure:?}"
        );
        assert!(failure.cleanup.is_none(), "cleanup not confirmed");
        let inspected = Command::new("docker")
            .args([
                "--host",
                &endpoint,
                "inspect",
                &instance.ok_or("missing instance")?,
            ])
            .output()?;
        assert!(!inspected.status.success());
        let diagnostic = String::from_utf8(inspected.stderr)?.to_lowercase();
        assert!(diagnostic.contains("no such object") || diagnostic.contains("no such container"));
    }
    Ok(())
}
