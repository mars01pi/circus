//! Real Docker integration tests. No mocks stand in for container isolation.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use circus_sandbox::{
    BackendConfig, CommandRequest, IoLimits, IsolationConfig, Ownership, ProcessExit,
    ProcessOutput, ProcessRequest, ProcessSession, ProcessTermination, ResourceLimits, Sandbox,
    SandboxConfig, SandboxError, SandboxPhase,
};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Fixture {
    sandbox: Sandbox,
    host: String,
    id: String,
}

impl Fixture {
    async fn new() -> Result<Self> {
        assert_eq!(std::env::var("CIRCUS_DOCKER_SANDBOX").as_deref(), Ok("1"));
        let backend = BackendConfig::docker(
            std::env::var("CIRCUS_DOCKER_ENDPOINT")?,
            std::env::var("CIRCUS_DOCKER_IMAGE").unwrap_or_else(|_| "python:3.12-slim".into()),
            "runc",
        );
        let host = match &backend {
            BackendConfig::Docker(docker) => match docker.endpoint() {
                endpoint if endpoint.starts_with('/') => format!("unix://{endpoint}"),
                endpoint => endpoint.to_string(),
            },
            BackendConfig::Containerd(_) => {
                panic!("docker fixture received a containerd descriptor")
            }
            BackendConfig::Kubernetes(_) => {
                panic!("docker fixture received a Kubernetes descriptor")
            }
        };
        let config = SandboxConfig::new(
            ResourceLimits::new(64 * 1024 * 1024, 1000, 32, 8 * 1024 * 1024, 1024 * 1024)?,
            IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)?,
            IoLimits::new(4 * 1024 * 1024, Duration::from_secs(10))?,
        )?;
        let sandbox = Sandbox::create(backend, config).await?;
        let id = sandbox.status().await?.instance_id.ok_or("missing id")?;
        let fixture = Self { sandbox, host, id };
        fixture.sandbox.start().await?;
        Ok(fixture)
    }

    fn assert_removed(&self) -> Result {
        let result = Command::new("docker")
            .args(["--host", &self.host, "inspect", &self.id])
            .output()?;
        assert!(!result.status.success());
        let stderr = String::from_utf8_lossy(&result.stderr).to_ascii_lowercase();
        assert!(
            stderr.contains("no such") || stderr.contains("not found"),
            "{stderr}"
        );
        Ok(())
    }

    // A fixture owns exactly one container. Teardown is explicit and has no
    // Drop fallback: `Sandbox::destroy` reports success only after confirmed
    // absence, then the engine must also report the container gone.
    async fn teardown(&self) -> Result {
        self.sandbox.destroy().await?;
        self.assert_removed()
    }
}

fn python(script: &str) -> ProcessRequest {
    ProcessRequest {
        argv: vec!["python3".into(), "-u".into(), "-c".into(), script.into()],
        working_dir: None,
        env: BTreeMap::new(),
        timeout: Duration::from_secs(20),
        output_limit_bytes: 64 * 1024,
    }
}

async fn until_stdout(process: &ProcessSession, expected: &[u8]) -> Result<Vec<u8>> {
    Ok(tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        while !bytes.ends_with(expected) {
            match process.next_output().await {
                Some(ProcessOutput::Stdout(chunk)) => bytes.extend(chunk),
                Some(ProcessOutput::Stderr(chunk)) => panic!("unexpected stderr: {chunk:?}"),
                None => panic!("output closed before expected response"),
            }
        }
        bytes
    })
    .await?)
}

async fn drain(process: &ProcessSession) -> (Vec<u8>, Vec<u8>) {
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    while let Some(output) = process.next_output().await {
        match output {
            ProcessOutput::Stdout(bytes) => stdout.extend(bytes),
            ProcessOutput::Stderr(bytes) => stderr.extend(bytes),
        }
    }
    (stdout, stderr)
}

fn assert_terminated(exit: ProcessExit, expected: ProcessTermination) {
    match exit {
        ProcessExit::Terminated { reason, cleanup } => {
            assert_eq!(reason, expected);
            assert_eq!(cleanup.ownership, Ownership::ConfirmedAbsent);
        }
        other => panic!("expected termination, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn interactive_round_trip_preserves_workspace_and_lifecycle() -> Result {
    let f = Fixture::new().await?;
    f.sandbox.create_dir_all("project").await?;
    let mut request = python(
        r#"
import os, sys
from pathlib import Path
print(os.getcwd() + ':' + os.environ['CIRCUS_MESSAGE'])
for line in sys.stdin:
    print('reply:' + line.strip())
Path('result.txt').write_text('finished')
print('diagnostic', file=sys.stderr)
sys.exit(7)
"#,
    );
    request.working_dir = Some("project".into());
    request
        .env
        .insert("CIRCUS_MESSAGE".into(), "explicit".into());
    let process = f.sandbox.spawn_process(request).await?;
    assert_eq!(
        until_stdout(&process, b"explicit\n").await?,
        b"/workspace/project:explicit\n"
    );

    // Both notification waits are cancellation-safe.
    assert!(
        tokio::time::timeout(Duration::from_millis(20), process.wait())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), process.next_output())
            .await
            .is_err()
    );
    for message in ["one", "two"] {
        process
            .write_stdin(format!("{message}\n").as_bytes())
            .await?;
        assert_eq!(
            until_stdout(&process, b"\n").await?,
            format!("reply:{message}\n").as_bytes()
        );
    }

    tokio::time::timeout(Duration::from_secs(5), async {
        assert_eq!(f.sandbox.status().await?.phase, SandboxPhase::Running);
        f.sandbox.write_file("live.txt", b"while running").await?;
        assert_eq!(f.sandbox.read_file("live.txt").await?, b"while running");
        f.sandbox.remove_file("live.txt").await?;
        f.sandbox.pause().await?;
        assert_eq!(f.sandbox.status().await?.phase, SandboxPhase::Paused);
        f.sandbox.resume().await?;
        Ok::<_, SandboxError>(())
    })
    .await??;
    assert!(matches!(
        f.sandbox.spawn_process(python("print(1)")).await,
        Err(SandboxError::ProcessActive)
    ));
    assert!(matches!(
        f.sandbox
            .exec(CommandRequest {
                argv: vec!["true".into()],
                timeout: Duration::from_secs(1),
                output_limit_bytes: 10,
            })
            .await,
        Err(SandboxError::ProcessActive)
    ));

    process.close_stdin().await?;
    let (_, stderr) = drain(&process).await;
    assert_eq!(stderr, b"diagnostic\n");
    assert_eq!(process.wait().await?, ProcessExit::Exited { exit_code: 7 });
    assert_eq!(process.wait().await?, ProcessExit::Exited { exit_code: 7 });
    assert_eq!(
        f.sandbox.read_file("project/result.txt").await?,
        b"finished"
    );

    let second = f.sandbox.spawn_process(python("print('second')")).await?;
    drop(process); // An old session must never cancel a new session.
    assert_eq!(drain(&second).await.0, b"second\n");
    assert_eq!(second.wait().await?, ProcessExit::Exited { exit_code: 0 });
    f.teardown().await
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn cancel_and_destroy_remove_descendants() -> Result {
    for use_destroy in [false, true] {
        let f = Fixture::new().await?;
        let process = f
            .sandbox
            .spawn_process(python(
                r#"
import subprocess, sys, time
subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(300)'])
print('ready')
time.sleep(300)
"#,
            ))
            .await?;
        until_stdout(&process, b"ready\n").await?;
        if use_destroy {
            tokio::time::timeout(Duration::from_secs(5), f.sandbox.destroy()).await??;
        } else {
            process.cancel();
        }
        assert_terminated(
            tokio::time::timeout(Duration::from_secs(5), process.wait()).await??,
            ProcessTermination::Cancelled,
        );
        assert_eq!(f.sandbox.status().await?.phase, SandboxPhase::Destroyed);
        f.teardown().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn timeout_and_output_limits_destroy_the_container() -> Result {
    for (script, timeout, reason) in [
        (
            "import time; time.sleep(300)",
            Duration::from_millis(200),
            ProcessTermination::TimedOut,
        ),
        (
            "import sys; sys.stdout.write('x' * 4096)",
            Duration::from_secs(5),
            ProcessTermination::OutputLimit { limit: 1024 },
        ),
        (
            "import sys; sys.stderr.write('x' * 4096)",
            Duration::from_secs(5),
            ProcessTermination::OutputLimit { limit: 1024 },
        ),
    ] {
        let f = Fixture::new().await?;
        let mut request = python(script);
        request.timeout = timeout;
        request.output_limit_bytes = 1024;
        let process = f.sandbox.spawn_process(request).await?;
        assert_terminated(
            tokio::time::timeout(Duration::from_secs(10), process.wait()).await??,
            reason,
        );
        f.teardown().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn full_output_queue_does_not_block_destroy() -> Result {
    let f = Fixture::new().await?;
    let mut request = python("import sys, time; sys.stdout.write('x' * 1048576); time.sleep(300)");
    request.output_limit_bytes = 2 * 1024 * 1024;
    let process = f.sandbox.spawn_process(request).await?;
    // Reading one chunk establishes output has started. Leave the rest undrained.
    assert!(process.next_output().await.is_some());
    tokio::time::timeout(Duration::from_secs(5), async {
        f.sandbox.write_file("responsive.txt", b"yes").await?;
        assert_eq!(f.sandbox.status().await?.phase, SandboxPhase::Running);
        f.sandbox.destroy().await
    })
    .await??;
    assert_terminated(process.wait().await?, ProcessTermination::Cancelled);
    f.teardown().await
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn dropping_session_cleans_only_its_container() -> Result {
    let f = Fixture::new().await?;
    let other = Fixture::new().await?;
    let process = f
        .sandbox
        .spawn_process(python("import time; print('ready'); time.sleep(300)"))
        .await?;
    until_stdout(&process, b"ready\n").await?;
    drop(process);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if f.sandbox.status().await?.phase == SandboxPhase::Destroyed {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok::<_, SandboxError>(())
    })
    .await??;
    f.teardown().await?;
    assert_eq!(other.sandbox.status().await?.phase, SandboxPhase::Running);
    other.teardown().await
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn cancelled_blocked_stdin_write_cleans_container() -> Result {
    let f = Fixture::new().await?;
    let process = f
        .sandbox
        .spawn_process(python("import time; print('ready'); time.sleep(300)"))
        .await?;
    until_stdout(&process, b"ready\n").await?;
    let bytes = vec![b'x'; 16 * 1024 * 1024];
    assert!(
        tokio::time::timeout(Duration::from_millis(200), process.write_stdin(&bytes))
            .await
            .is_err()
    );
    assert_terminated(
        tokio::time::timeout(Duration::from_secs(10), process.wait()).await??,
        ProcessTermination::Cancelled,
    );
    f.teardown().await
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn rejected_process_request_keeps_sandbox_usable() -> Result {
    let f = Fixture::new().await?;
    for request in [
        ProcessRequest {
            argv: vec![],
            ..python("print(1)")
        },
        ProcessRequest {
            working_dir: Some("../escape".into()),
            ..python("print(1)")
        },
        ProcessRequest {
            output_limit_bytes: 5 * 1024 * 1024,
            ..python("print(1)")
        },
        ProcessRequest {
            env: BTreeMap::from([("BAD=NAME".into(), "value".into())]),
            ..python("print(1)")
        },
    ] {
        assert!(f.sandbox.spawn_process(request).await.is_err());
        assert_eq!(f.sandbox.status().await?.phase, SandboxPhase::Running);
    }
    f.teardown().await
}
