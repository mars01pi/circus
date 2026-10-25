//! Real containerd interactive process suite: image environment inheritance,
//! streaming, stdin, exit, and whole-container termination outcomes.

mod containerd_support;

use std::collections::BTreeMap;
use std::time::Duration;

use circus_sandbox::{
    Ownership, ProcessExit, ProcessOutput, ProcessRequest, ProcessSession, ProcessTermination,
    SandboxPhase,
};
use containerd_support as support;

fn process_request(script: &str, output_limit: usize, timeout: Duration) -> ProcessRequest {
    ProcessRequest {
        argv: vec![
            "python3".to_string(),
            "-u".to_string(),
            "-c".to_string(),
            script.to_string(),
        ],
        working_dir: None,
        env: BTreeMap::new(),
        timeout,
        output_limit_bytes: output_limit,
    }
}

async fn until_stdout(process: &ProcessSession, expected: &[u8]) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut bytes = Vec::new();
        while !bytes.ends_with(expected) {
            match process.next_output().await {
                Some(ProcessOutput::Stdout(chunk)) => bytes.extend(chunk),
                Some(ProcessOutput::Stderr(_)) => {}
                None => panic!("output closed before expected bytes"),
            }
        }
        bytes
    })
    .await
    .expect("expected stdout within the deadline")
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

#[tokio::test]
#[ignore = "requires an explicit real containerd daemon"]
async fn containerd_interactive_process_round_trip() -> support::Result {
    support::enabled();
    let sandbox = support::create().await?;
    let id = sandbox.status().await?.instance_id.ok_or("missing id")?;
    sandbox.create_dir_all("project").await?;
    sandbox.start().await?;

    let mut request = process_request(
        "import os, sys\nfrom pathlib import Path\nprint('ready:' + os.getcwd())\nfor line in sys.stdin:\n    print('reply:' + line.strip(), flush=True)\nPath('result.txt').write_text('finished')\nprint('diagnostic', file=sys.stderr, flush=True)\nsys.exit(7)\n",
        64 * 1024,
        Duration::from_secs(20),
    );
    // The lifecycle resolves working_dir below the configured workspace.
    request.working_dir = Some("project".to_string());
    let process = sandbox.spawn_process(request).await?;
    assert_eq!(
        until_stdout(&process, b"ready:/workspace/project\n").await,
        b"ready:/workspace/project\n"
    );

    // Files and status stay usable while the process runs.
    assert_eq!(sandbox.status().await?.phase, SandboxPhase::Running);
    sandbox.write_file("while-running.txt", b"parallel").await?;

    for message in ["one", "two"] {
        process
            .write_stdin(format!("{message}\n").as_bytes())
            .await?;
        assert_eq!(
            until_stdout(&process, b"\n").await,
            format!("reply:{message}\n").as_bytes()
        );
    }
    process.close_stdin().await?;
    let (_, stderr) = drain(&process).await;
    assert_eq!(stderr, b"diagnostic\n");
    assert_eq!(process.wait().await?, ProcessExit::Exited { exit_code: 7 });

    // A normal exit preserves the container and its workspace.
    assert_eq!(sandbox.status().await?.phase, SandboxPhase::Running);
    assert_eq!(sandbox.read_file("project/result.txt").await?, b"finished");
    assert_eq!(sandbox.read_file("while-running.txt").await?, b"parallel");

    support::destroy_and_assert(&sandbox, &id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires real containerd and an official Python image environment"]
async fn containerd_process_inherits_image_environment() -> support::Result {
    // This also requires the host flag to be exactly "1", providing a positive
    // control without changing the process-global host environment.
    support::enabled();
    let sandbox = support::create().await?;
    let mut id = None;
    let result: support::Result<_> = async {
        id = sandbox.status().await?.instance_id;
        if id.is_none() {
            return Err("missing id".into());
        }
        sandbox.start().await?;
        let script = r#"import json, os, sys
print(json.dumps({
    'image_version': os.environ.get('PYTHON_VERSION'),
    'runtime_version': sys.version.split()[0],
    'host_flag': os.environ.get('CIRCUS_CONTAINERD'),
    'custom_value': os.environ.get('CIRCUS_CONTAINERD_ENV_REGRESSION'),
}))
print('environment-diagnostic', file=sys.stderr)
sys.exit(7)
"#;
        let mut outputs = Vec::with_capacity(2);
        for env in [
            BTreeMap::new(),
            BTreeMap::from([
                (
                    "PYTHON_VERSION".to_string(),
                    "request=version=override".to_string(),
                ),
                (
                    "CIRCUS_CONTAINERD_ENV_REGRESSION".to_string(),
                    "request=value=with=equals".to_string(),
                ),
            ]),
        ] {
            let mut request = process_request(script, 64 * 1024, Duration::from_secs(20));
            request.env = env;
            let process = sandbox.spawn_process(request).await?;
            process.close_stdin().await?;
            let (stdout, stderr) = drain(&process).await;
            outputs.push((stdout, stderr, process.wait().await?));
        }
        // CommandRequest has no explicit env field, but exec inherits the same
        // stored image environment after both interactive sessions finish.
        let synchronous = sandbox.exec(support::python(&["-c", script])).await?;
        Ok((outputs, synchronous))
    }
    .await;
    // Cleanup runs before propagating operation errors or checking guest output.
    sandbox.destroy().await?;
    if let Some(id) = id {
        support::assert_absent(&support::namespace(), &id)?;
    }
    let (outputs, synchronous) = result?;
    assert_eq!(synchronous.exit_code, 7);
    assert_eq!(synchronous.stderr, b"environment-diagnostic\n");
    let synchronous_stdout: serde_json::Value = serde_json::from_slice(&synchronous.stdout)?;
    let runtime_version = synchronous_stdout["runtime_version"]
        .as_str()
        .ok_or("missing interpreter version")?;
    let inherited = serde_json::json!({
        "image_version": runtime_version,
        "runtime_version": runtime_version,
        "host_flag": null,
        "custom_value": null,
    });
    assert_eq!(synchronous_stdout, inherited);
    let overridden = serde_json::json!({
        "image_version": "request=version=override",
        "runtime_version": runtime_version,
        "host_flag": null,
        "custom_value": "request=value=with=equals",
    });
    for ((stdout, stderr, exit), expected) in outputs.into_iter().zip([inherited, overridden]) {
        assert_eq!(exit, ProcessExit::Exited { exit_code: 7 });
        assert_eq!(stderr, b"environment-diagnostic\n");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stdout)?,
            expected
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real containerd daemon"]
async fn containerd_process_cancellation_removes_the_container() -> support::Result {
    support::enabled();
    let sandbox = support::create().await?;
    let id = sandbox.status().await?.instance_id.ok_or("missing id")?;
    sandbox.start().await?;

    let process = sandbox
        .spawn_process(process_request(
            "import time\nprint('sleeping', flush=True)\ntime.sleep(120)\n",
            64 * 1024,
            Duration::from_secs(60),
        ))
        .await?;
    until_stdout(&process, b"sleeping\n").await;
    process.cancel();
    match tokio::time::timeout(Duration::from_secs(20), process.wait())
        .await
        .expect("process outcome within the deadline")?
    {
        ProcessExit::Terminated { reason, cleanup } => {
            assert_eq!(reason, ProcessTermination::Cancelled);
            assert_eq!(cleanup.ownership, Ownership::ConfirmedAbsent);
        }
        other => panic!("expected cancellation, got {other:?}"),
    }
    assert_eq!(sandbox.status().await?.phase, SandboxPhase::Destroyed);
    support::assert_absent(&support::namespace(), &id)?;
    assert!(sandbox.destroy().await.is_ok());
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real containerd daemon"]
async fn containerd_process_output_limit_removes_the_container() -> support::Result {
    support::enabled();
    let sandbox = support::create().await?;
    let id = sandbox.status().await?.instance_id.ok_or("missing id")?;
    sandbox.start().await?;

    let process = sandbox
        .spawn_process(process_request(
            "import sys\nsys.stderr.write('x' * 200000)\nsys.stderr.flush()\nimport time\ntime.sleep(120)\n",
            1024,
            Duration::from_secs(60),
        ))
        .await?;
    match tokio::time::timeout(Duration::from_secs(20), process.wait())
        .await
        .expect("process outcome within the deadline")?
    {
        ProcessExit::Terminated { reason, cleanup } => {
            assert_eq!(reason, ProcessTermination::OutputLimit { limit: 1024 });
            assert_eq!(cleanup.ownership, Ownership::ConfirmedAbsent);
        }
        other => panic!("expected output-limit termination, got {other:?}"),
    }
    assert_eq!(sandbox.status().await?.phase, SandboxPhase::Destroyed);
    support::assert_absent(&support::namespace(), &id)?;
    assert!(sandbox.destroy().await.is_ok());
    Ok(())
}
