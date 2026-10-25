use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde::Deserialize;

struct Peer {
    process: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    _directory: tempfile::TempDir,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Frame {
    #[serde(rename = "type")]
    kind: String,
    code: Option<String>,
    task_id: Option<String>,
    result: Option<ResultFrame>,
    event: Option<EventFrame>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct EventFrame {
    instance_id: Option<String>,
}

#[derive(Deserialize, Debug)]
struct ResultFrame {
    status: String,
    cleanup: String,
    error: Option<FailureFrame>,
}

#[derive(Deserialize, Debug)]
struct FailureFrame {
    code: String,
    message: String,
}

impl Peer {
    fn new() -> Self {
        let endpoint = if cfg!(windows) {
            "npipe:////./pipe/circus-stdio-missing-engine"
        } else {
            "unix:///circus-stdio-missing-engine.sock"
        };
        Self::with_backend(
            endpoint,
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &std::env::current_exe().unwrap(),
        )
    }

    fn with_backend(endpoint: &str, image: &str, python: &std::path::Path) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let credential = directory.path().join("key");
        std::fs::write(&credential, "host-only-test-sentinel").unwrap();
        let config = directory.path().join("config.json");
        // This endpoint must fail before allocating a sandbox; no fake runner.
        let json = format!(
            r#"{{"version":1,"dockerEndpoint":{},"agentImage":{},"python":{},"resources":{{"memoryBytes":536870912,"cpuMillis":1000,"maxProcesses":128,"workspaceBytes":134217728,"maxFileBytes":2097152}},"model":{{"baseUrl":"https://unused.invalid/v1","model":"test","apiKeyFile":{}}}}}"#,
            sonic_rs::to_string(endpoint).unwrap(),
            sonic_rs::to_string(image).unwrap(),
            sonic_rs::to_string(python).unwrap(),
            sonic_rs::to_string(&credential).unwrap()
        );
        std::fs::write(&config, json).unwrap();
        let mut process = Command::new(env!("CARGO_BIN_EXE_circus-stdio-bridge"))
            .args(["--config", config.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = process.stdin.take();
        let output = BufReader::new(process.stdout.take().unwrap());
        let mut peer = Self {
            process,
            input,
            output,
            _directory: directory,
        };
        assert_eq!(peer.read().kind, "ready");
        peer
    }

    fn write(&mut self, frame: &str) {
        self.input
            .as_mut()
            .unwrap()
            .write_all(frame.as_bytes())
            .unwrap();
    }
    fn read(&mut self) -> Frame {
        let mut line = String::new();
        assert!(self.output.read_line(&mut line).unwrap() > 0);
        assert!(!line.contains("host-only-test-sentinel"));
        sonic_rs::from_str(&line).unwrap()
    }
    fn close(&mut self) {
        drop(self.input.take());
    }
    fn wait(&mut self) -> std::process::ExitStatus {
        for _ in 0..500 {
            if let Some(status) = self.process.try_wait().unwrap() {
                return status;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        self.process.kill().unwrap();
        panic!("bridge did not exit after stdin EOF");
    }
}

#[test]
#[ignore = "requires explicit real Linux Docker and the pinned Codex image"]
fn docker_cancellation_and_eof_confirm_owned_container_removal() {
    assert_eq!(std::env::var("CIRCUS_DOCKER_SANDBOX").as_deref(), Ok("1"));
    let endpoint = std::env::var("CIRCUS_DOCKER_ENDPOINT").unwrap();
    let image = std::env::var("CIRCUS_AGENT_IMAGE").unwrap();
    let python = std::env::var("CIRCUS_HOST_PYTHON").unwrap();
    for cancel in [true, false] {
        let mut peer = Peer::with_backend(&endpoint, &image, std::path::Path::new(&python));
        peer.write(&START.replace("\"timeoutMs\":1000", "\"timeoutMs\":60000"));
        let task_id = peer.read().task_id.unwrap();
        let instance = loop {
            let frame = peer.read();
            assert_ne!(
                frame.kind, "result",
                "task ended before sandbox creation: {:?}",
                frame.result
            );
            if let Some(EventFrame {
                instance_id: Some(id),
            }) = frame.event
            {
                break id;
            }
        };
        if cancel {
            peer.write(&format!("{{\"v\":1,\"type\":\"cancel\",\"requestId\":\"client-a\",\"taskId\":\"{task_id}\"}}\n"));
        } else {
            peer.close();
        }
        loop {
            let frame = peer.read();
            if frame.kind == "result" {
                let result = frame.result.unwrap();
                assert_eq!(result.status, "cancelled");
                assert_eq!(result.cleanup, "confirmed");
                let error = result.error.unwrap();
                assert_eq!(error.code, "cancelled");
                assert_eq!(error.message, "Agent task cancelled");
                break;
            }
        }
        peer.close();
        assert!(peer.wait().success());
        let inspection = Command::new("docker")
            .args(["--host", &endpoint, "inspect", &instance])
            .output()
            .unwrap();
        assert!(!inspection.status.success());
        assert!(
            String::from_utf8(inspection.stderr)
                .unwrap()
                .to_lowercase()
                .contains("no such")
        );
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

const START: &str = "{\"v\":1,\"type\":\"start\",\"requestId\":\"client-a\",\"request\":{\"prompt\":\"test\",\"inputFiles\":[],\"outputPaths\":[],\"timeoutMs\":1000}}\n";

#[test]
fn real_entry_reports_backend_failure_without_claiming_success() {
    let mut peer = Peer::new();
    peer.write(START);
    assert_eq!(peer.read().kind, "accepted");
    loop {
        let frame = peer.read();
        if frame.kind == "result" {
            let result = frame.result.unwrap();
            assert_eq!(result.status, "failed");
            assert_eq!(result.cleanup, "confirmed");
            let error = result.error.unwrap();
            assert_eq!(error.code, "sandbox");
            assert_eq!(
                error.message,
                "Agent task failed; inspect the error code and cleanup status"
            );
            break;
        }
    }
    peer.close();
    assert!(peer.wait().success());
}

#[test]
fn foreign_task_identity_is_rejected_and_owner_is_settled() {
    let mut peer = Peer::new();
    peer.write(START);
    let accepted = peer.read();
    assert_eq!(accepted.kind, "accepted");
    assert!(accepted.task_id.is_some());
    peer.write(
        "{\"v\":1,\"type\":\"cancel\",\"requestId\":\"client-b\",\"taskId\":\"another-task\"}\n",
    );
    loop {
        let frame = peer.read();
        if frame.kind == "error" {
            assert_eq!(frame.code.as_deref(), Some("wrong_task"));
            break;
        }
    }
    assert!(!peer.wait().success());
}

#[test]
fn malformed_and_truncated_frames_fail_closed() {
    for invalid in [
        "{no-json}\n",
        "{\"v\":1",
        "{\"v\":2,\"type\":\"cancel\",\"requestId\":\"a\",\"taskId\":\"b\"}\n",
    ] {
        let mut peer = Peer::new();
        peer.write(invalid);
        peer.close();
        let frame = peer.read();
        assert_eq!(frame.kind, "error");
        assert_eq!(frame.code.as_deref(), Some("invalid_frame"));
        assert!(!peer.wait().success());
    }
}

#[test]
fn invalid_path_never_starts_a_task() {
    let mut peer = Peer::new();
    peer.write(&START.replace("\"outputPaths\":[]", "\"outputPaths\":[\"../host\"]"));
    assert_eq!(peer.read().code.as_deref(), Some("invalid_request"));
    assert!(!peer.wait().success());
}
