use std::collections::BTreeMap;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use circus_sandbox::{BackendConfig, ResourceLimits, SnapshotBudget};

use crate::runner::BRIDGE;
use crate::{
    AgentRunner, AgentTask, ModelConfig, ModelFailure, RunnerConfig, TaskError, TaskEvent,
    TaskFailure, TaskOutcome, TaskPhase, TaskRequest, TaskWorkspace, ToolPhase,
};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn runner() -> Result<AgentRunner> {
    assert_eq!(std::env::var("CIRCUS_DOCKER_SANDBOX").as_deref(), Ok("1"));
    Ok(AgentRunner::new(RunnerConfig {
        backend: BackendConfig::docker(
            std::env::var("CIRCUS_DOCKER_ENDPOINT")?,
            std::env::var("CIRCUS_AGENT_IMAGE")?,
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
            "host-only-test-sentinel".into(),
        )?,
        python: std::env::var_os("CIRCUS_HOST_PYTHON")
            .unwrap_or_else(|| "python3".into())
            .into(),
    })?)
}

fn request(marker: &str) -> TaskRequest {
    TaskRequest {
        prompt: "Fix answer.py, run the Python assertions, and write result.txt.".into(),
        input_files: BTreeMap::from([
            (
                "answer.py".into(),
                b"def add(a, b): return a - b\n".to_vec(),
            ),
            ("marker.txt".into(), marker.as_bytes().to_vec()),
        ]),
        output_paths: vec!["answer.py".into(), "result.txt".into(), "marker.txt".into()],
        timeout: Duration::from_secs(60),
        workspace: None,
        agent_session: None,
    }
}

fn fixture() -> String {
    format!(
        "{BRIDGE}\n{}",
        include_str!("../../tests/fixtures/codex_responses.py")
    )
}

fn instance_absent(id: &str) -> Result<bool> {
    let output = Command::new("docker")
        .args([
            "--host",
            &std::env::var("CIRCUS_DOCKER_ENDPOINT")?,
            "inspect",
            id,
        ])
        .output()?;
    if output.status.success() {
        return Ok(false);
    }
    let error = String::from_utf8(output.stderr)?.to_lowercase();
    if error.contains("no such object") || error.contains("no such container") {
        return Ok(true);
    }
    Err(format!("cannot confirm test-owned instance absence: {error}").into())
}

async fn collect(task: &AgentTask) -> Result<(Arc<TaskOutcome>, Vec<TaskEvent>)> {
    tokio::time::timeout(Duration::from_secs(100), async {
        let mut events = Vec::new();
        while let Some(event) = task.next_event().await {
            events.push(event);
        }
        let result = task.wait().await;
        let id = events
            .iter()
            .find_map(|event| match event {
                TaskEvent::SandboxCreated { instance_id } => Some(instance_id),
                _ => None,
            })
            .ok_or_else(|| format!("missing task-owned instance identity: {result:?}"))?;
        assert!(instance_absent(id)?, "test-owned container leaked");
        Ok((result, events))
    })
    .await?
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker and the pinned Codex image"]
async fn real_tasks_edit_inputs_emit_events_export_artifacts_and_isolate_concurrent_runs() -> Result
{
    let runner = runner()?;
    let first = runner.start_controller(request("first"), fixture())?;
    let second = runner.start_controller(request("second"), fixture())?;
    let (first_result, second_result) = tokio::join!(collect(&first), collect(&second));
    for (result, marker) in [(first_result?, "first"), (second_result?, "second")] {
        let (outcome, events) = result;
        let output = outcome
            .as_ref()
            .as_ref()
            .map_err(|e| format!("{e:?}; fixture events: {events:?}"))?;
        assert_eq!(output.artifacts["result.txt"], b"42\n");
        assert_eq!(
            output.artifacts["answer.py"],
            b"def add(a, b): return a + b\n"
        );
        assert_eq!(output.artifacts["marker.txt"], marker.as_bytes());
        assert!(events.iter().any(|e| matches!(
            e,
            TaskEvent::Tool {
                phase: ToolPhase::Started,
                ..
            }
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            TaskEvent::Tool {
                phase: ToolPhase::Completed,
                exit_code: Some(0),
                ..
            }
        )));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TaskEvent::TextDelta { text } if text.contains("result.txt")))
        );
        assert!(events.iter().any(
            |e| matches!(e, TaskEvent::ToolOutput { text, .. } if text.contains("tests passed"))
        ));
    }
    assert!(Arc::ptr_eq(&first.wait().await, &first.wait().await));
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker and the pinned Codex image"]
async fn model_failure_keeps_safe_http_status_and_missing_artifact_is_not_success() -> Result {
    let rejected = format!(
        "{BRIDGE}\nclass RejectPeer:\n def forward(self, body, emit):\n  raise BridgeError('secret upstream body', 'rejected', 403)\nmain_host(RejectPeer())\n"
    );
    let task = runner()?.start_controller(request("rejected"), rejected)?;
    let (outcome, _) = collect(&task).await?;
    let failure = outcome.as_ref().as_ref().unwrap_err();
    assert!(matches!(
        failure.error,
        TaskError::Model(ModelFailure::Rejected { status: 403 })
    ));
    assert!(failure.cleanup.is_none());
    assert!(!format!("{failure:?}").contains("secret upstream"));
    let mut missing = request("missing");
    missing.output_paths.push("missing.txt".into());
    let task = runner()?.start_controller(missing, fixture())?;
    let (outcome, events) = collect(&task).await?;
    assert!(
        matches!(
            outcome.as_ref(),
            Err(crate::TaskFailure {
                error: TaskError::Sandbox {
                    phase: TaskPhase::Exporting,
                    ..
                },
                cleanup: None
            })
        ),
        "{outcome:?}; fixture events: {events:?}"
    );
    Ok(())
}

fn stuck_driver(path: &std::path::Path) -> Result<String> {
    let path = sonic_rs::to_string(path.to_str().ok_or("non-UTF-8 fixture path")?)?;
    Ok(format!(
        "import os, time, sys\nfrom pathlib import Path\nPath({path}).write_text(str(os.getpid()))\nprint('{{\"kind\":\"running\"}}', file=sys.stderr, flush=True)\ntime.sleep(300)\n"
    ))
}

fn host_absent(path: &std::path::Path) -> Result {
    let pid: u32 = std::fs::read_to_string(path)?.parse()?;
    assert!(pid > 0);
    #[cfg(unix)]
    {
        let python = std::env::var_os("CIRCUS_HOST_PYTHON").unwrap_or_else(|| "python3".into());
        let status = Command::new(python).args(["-I", "-B", "-c", "import os, sys\ntry: os.kill(int(sys.argv[1]), 0)\nexcept ProcessLookupError: sys.exit(0)\nsys.exit(1)", &pid.to_string()]).status()?;
        assert!(status.success(), "host controller still exists");
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker and the pinned Codex image"]
async fn cancellation_timeout_and_handle_drop_reap_owned_resources() -> Result {
    for mode in ["cancel", "timeout", "drop"] {
        let pid = tempfile::NamedTempFile::new()?;
        let mut request = request(mode);
        request.timeout = Duration::from_secs(if mode == "timeout" { 5 } else { 60 });
        let task = runner()?.start_controller(request, stuck_driver(pid.path())?)?;
        let mut owned = None;
        tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(event) = task.next_event().await {
                match event {
                    TaskEvent::SandboxCreated { instance_id } => owned = Some(instance_id),
                    TaskEvent::Phase(TaskPhase::Running) => return,
                    _ => {}
                }
            }
            panic!("controller never started: {:?}", task.wait().await);
        })
        .await?;
        let id = owned.ok_or("missing ownership identity")?;
        if mode == "drop" {
            drop(task);
            tokio::time::timeout(Duration::from_secs(30), async {
                while !instance_absent(&id)? {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Ok::<_, Box<dyn std::error::Error>>(())
            })
            .await??;
        } else {
            if mode == "cancel" {
                task.cancel();
                task.cancel();
            }
            while task.next_event().await.is_some() {}
            let outcome = task.wait().await;
            let failure = outcome.as_ref().as_ref().unwrap_err();
            assert!(
                matches!(
                    (&failure.error, mode),
                    (TaskError::Cancelled, "cancel") | (TaskError::TimedOut, "timeout")
                ),
                "{failure:?}"
            );
            assert!(failure.cleanup.is_none());
            assert!(instance_absent(&id)?);
        }
        host_absent(pid.path())?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker and the pinned Codex image"]
async fn sequential_tasks_restore_workspace_and_resume_the_native_codex_thread() -> Result {
    let runner = runner()?;
    let directory = tempfile::tempdir()?;
    let archive = directory.path().join("workspace.archive");
    let workspace = || TaskWorkspace {
        archive: archive.clone(),
        budget: SnapshotBudget::new(16 * 1024 * 1024, 2048).unwrap(),
    };
    let mut first = request("first");
    first.workspace = Some(workspace());
    first.output_paths.push("persisted.txt".into());
    let task = runner.start_controller(first, fixture())?;
    let (outcome, events) = collect(&task).await?;
    let output = outcome
        .as_ref()
        .as_ref()
        .map_err(|error| format!("{error:?}; fixture events: {events:?}"))?;
    let session = output
        .agent_session
        .clone()
        .ok_or("first task did not report a native agent session")?;
    assert_eq!(output.artifacts["persisted.txt"], b"durable\n");
    assert!(archive.is_file(), "workspace archive was not saved");

    let mut second = request("second");
    second.workspace = Some(workspace());
    second.agent_session = Some(session.clone());
    second.output_paths.push("persisted.txt".into());
    let task = runner.start_controller(second, fixture())?;
    let (outcome, events) = collect(&task).await?;
    let output = outcome
        .as_ref()
        .as_ref()
        .map_err(|error| format!("{error:?}; fixture events: {events:?}"))?;
    // The fixture only succeeds when it finds the first turn's file and the
    // second task resumed the same native thread instead of starting a new one.
    assert_eq!(output.artifacts["persisted.txt"], b"durable\n");
    assert_eq!(output.agent_session.as_deref(), Some(session.as_str()));
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker and the pinned Codex image"]
async fn workspace_archive_failures_are_typed_and_still_destroy_the_sandbox() -> Result {
    let runner = runner()?;
    let directory = tempfile::tempdir()?;
    let budget = SnapshotBudget::new(1024 * 1024, 128).unwrap();
    for (archive, expected) in [
        (
            directory.path().join("missing").join("workspace.archive"),
            TaskPhase::Exporting,
        ),
        (directory.path().to_path_buf(), TaskPhase::Preparing),
    ] {
        let mut request = request("archive");
        request.workspace = Some(TaskWorkspace { archive, budget });
        let task = runner.start_controller(request, fixture())?;
        let (outcome, events) = collect(&task).await?;
        let TaskFailure {
            error,
            cleanup: None,
        } = outcome.as_ref().as_ref().unwrap_err()
        else {
            return Err(format!("archive failure was not cleaned up: {outcome:?}").into());
        };
        assert!(
            matches!(
                error,
                TaskError::Workspace {
                    phase,
                    task: None,
                    ..
                } if *phase == expected
            ),
            "{error:?}; fixture events: {events:?}"
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker and the pinned Codex image"]
async fn overloaded_event_consumer_still_gets_outcome_and_owned_cleanup() -> Result {
    let pid = tempfile::NamedTempFile::new()?;
    let controller = stuck_driver(pid.path())?.replace(
        "time.sleep(300)",
        "for _ in range(1024): print('{\"kind\":\"text\",\"text\":\"fixture\"}', file=sys.stderr, flush=True)\ntime.sleep(300)",
    );
    let task = runner()?.start_controller(request("overload"), controller)?;
    // Intentionally stop consuming after ownership is known. The independent
    // result channel must still complete after the bounded event queue fills.
    let id = loop {
        match task.next_event().await {
            Some(TaskEvent::SandboxCreated { instance_id }) => break instance_id,
            Some(_) => {}
            None => return Err(format!("task did not start: {:?}", task.wait().await).into()),
        }
    };
    let outcome = tokio::time::timeout(Duration::from_secs(30), task.wait()).await?;
    assert!(
        matches!(
            outcome.as_ref(),
            Err(crate::TaskFailure {
                error: TaskError::EventBackpressure,
                cleanup: None
            })
        ),
        "{outcome:?}"
    );
    assert!(instance_absent(&id)?);
    host_absent(pid.path())?;
    Ok(())
}
