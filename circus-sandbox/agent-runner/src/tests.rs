mod claude;
mod docker;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use circus_sandbox::{BackendConfig, ResourceLimits, SnapshotBudget};
use tokio::io::BufReader;

use crate::events::{EVENT_QUEUE, Events, MAX_EVENT_FRAME, WireEvent};
use crate::{
    AgentKind, AgentRunner, ModelConfig, RunnerConfig, TaskError, TaskEvent, TaskRequest,
    TaskWorkspace,
};

fn request() -> TaskRequest {
    TaskRequest {
        prompt: "Fix the test".into(),
        input_files: BTreeMap::from([("src/main.py".into(), b"pass\n".to_vec())]),
        output_paths: vec!["src/main.py".into()],
        timeout: Duration::from_secs(30),
        workspace: None,
        agent_session: None,
    }
}

#[test]
fn task_paths_budgets_and_duplicates_are_rejected_before_allocation() {
    request().validate(1024).unwrap();
    for invalid in [
        "",
        "/etc/passwd",
        "../secret",
        "a/../b",
        "a//b",
        "a/./b",
        "C:secret",
        "a\\b",
        "a\0b",
    ] {
        let mut task = request();
        task.output_paths = vec![invalid.into()];
        assert!(
            matches!(task.validate(1024), Err(TaskError::Invalid(_))),
            "{invalid:?}"
        );
        task = request();
        task.input_files.insert(invalid.into(), vec![1]);
        assert!(task.validate(1024).is_err());
    }
    let mut task = request();
    task.output_paths.push(task.output_paths[0].clone());
    assert!(task.validate(1024).is_err());
    task = request();
    task.input_files.insert("large".into(), vec![0; 1025]);
    assert!(task.validate(1024).is_err());
    task = request();
    task.timeout = Duration::ZERO;
    assert!(task.validate(1024).is_err());
}

#[test]
fn model_configuration_is_explicit_and_debug_does_not_expose_credentials() {
    let model = ModelConfig::new(
        "https://gateway.example/v1".into(),
        "model".into(),
        "private-test-key".into(),
    )
    .unwrap();
    assert!(!format!("{model:?}").contains("private-test-key"));
    for url in [
        "http://gateway.example/v1",
        "https://user:secret@gateway.example/v1",
        "https://gateway.example/v1?key=secret",
        "https://gateway.example/v1#secret",
    ] {
        assert!(ModelConfig::new(url.into(), "model".into(), "key".into()).is_err());
    }
}

#[tokio::test]
async fn event_queue_and_frames_have_hard_limits() {
    let (sender, _receiver) = tokio::sync::mpsc::channel(EVENT_QUEUE);
    let mut events = Events::new(sender);
    for _ in 0..EVENT_QUEUE {
        events
            .send(TaskEvent::TextDelta {
                text: "hello".into(),
            })
            .unwrap();
    }
    assert!(matches!(
        events.send(TaskEvent::TextDelta {
            text: "overflow".into()
        }),
        Err(TaskError::EventBackpressure)
    ));
    let oversized = vec![b'x'; MAX_EVENT_FRAME + 1];
    assert!(
        crate::process::read_frame(&mut BufReader::new(oversized.as_slice()))
            .await
            .is_err()
    );
    assert!(
        crate::process::read_frame(&mut BufReader::new(&b"{}"[..]))
            .await
            .is_err()
    );
    for invalid in [
        b"{\"kind\":\"text\",\"text\":\"a\",\"text\":\"b\"}".as_slice(),
        b"{\"kind\":\"text\",\"text\":NaN}",
        b"\xff",
    ] {
        assert!(sonic_rs::from_slice::<WireEvent>(invalid).is_err());
    }
}

#[test]
fn workspace_and_agent_session_are_bounded_before_allocation() {
    let budget = SnapshotBudget::new(1024, 8).unwrap();
    let mut task = request();
    task.workspace = Some(TaskWorkspace {
        archive: PathBuf::new(),
        budget,
    });
    assert!(matches!(
        task.validate(1024),
        Err(TaskError::Invalid("workspace archive path"))
    ));
    for invalid in ["", "a b", "thread/one", "C:\\thread"] {
        let mut task = request();
        task.agent_session = Some(invalid.into());
        assert!(
            matches!(task.validate(1024), Err(TaskError::Invalid(_))),
            "{invalid:?}"
        );
    }
    let mut task = request();
    task.agent_session = Some("x".repeat(129));
    assert!(matches!(task.validate(1024), Err(TaskError::Invalid(_))));
    let mut task = request();
    task.workspace = Some(TaskWorkspace {
        archive: PathBuf::from("workspace.archive"),
        budget,
    });
    task.agent_session = Some("0199-abcd_EF".into());
    task.validate(1024).unwrap();
    let debug = format!("{:?}", task.workspace.as_ref().unwrap());
    assert!(!debug.contains("workspace.archive"));
    assert!(debug.contains("budget"));
}

#[test]
fn malformed_agent_session_wire_events_fail_closed() {
    let (sender, _receiver) = tokio::sync::mpsc::channel(EVENT_QUEUE);
    let mut events = Events::new(sender);
    let event: WireEvent =
        sonic_rs::from_str(r#"{"kind":"agent_session","id":"thread-1"}"#).unwrap();
    event.apply(&mut events).unwrap();
    assert_eq!(events.agent_session(), Some("thread-1"));
    for invalid in ["", "/tmp/x", "thread/1", "a b"] {
        let wire = format!(r#"{{"kind":"agent_session","id":"{invalid}"}}"#);
        let event: WireEvent = sonic_rs::from_str(&wire).unwrap();
        assert!(matches!(event.apply(&mut events), Err(TaskError::Protocol)));
    }
    let other: WireEvent =
        sonic_rs::from_str(r#"{"kind":"agent_session","id":"thread-2"}"#).unwrap();
    assert!(matches!(other.apply(&mut events), Err(TaskError::Protocol)));
}

#[test]
fn claude_agent_session_is_rejected_without_creating_a_sandbox() {
    let runner = AgentRunner::new(RunnerConfig {
        backend: BackendConfig::docker("unix:///unused/docker.sock", "unused-image", "runc"),
        resources: ResourceLimits::new(1024 * 1024, 1000, 32, 1024 * 1024, 4096).unwrap(),
        model: ModelConfig::new(
            "https://unused.invalid/v1".into(),
            "model".into(),
            "unused-key".into(),
        )
        .unwrap(),
        python: "python3".into(),
    })
    .unwrap()
    .with_agent(AgentKind::ClaudeCode);
    let mut task = request();
    task.agent_session = Some("thread-1".into());
    assert!(matches!(
        runner.start_controller(task, "unused".into()),
        Err(TaskError::Invalid(
            "agent session resume is unsupported for this agent"
        ))
    ));
}
