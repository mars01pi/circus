use super::*;
use std::{io::Write, process::Command};

const HELPER: &str = "infrastructure::git_process::tests::process_helper";

// Actual copies of the test executable provide controlled process trees, not
// mocked Git or SDK behavior. The marker is an externally observable heartbeat.
#[test]
fn process_helper() {
    let Ok(mode) = std::env::var("CIRCUS_PROCESS_HELPER") else {
        return;
    };
    if mode == "output" {
        std::io::stdout().write_all(&[b'o'; 800]).unwrap();
        std::io::stderr().write_all(&[b'e'; 800]).unwrap();
        return;
    }
    let marker = std::env::var_os("CIRCUS_PROCESS_MARKER").unwrap();
    if mode == "descendant" {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(marker)
            .unwrap();
        loop {
            file.write_all(b"alive\n").unwrap();
            file.flush().unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let mut descendant = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", HELPER, "--nocapture"])
        .env("CIRCUS_PROCESS_HELPER", "descendant")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !std::path::Path::new(&marker).exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "descendant did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    if mode == "exit" {
        // This deliberately leaves a non-child descendant alive after leader
        // completion; a leader-only wait or ECHILD check cannot pass the test.
        std::process::exit(0);
    }
    let _ = descendant.wait();
}

fn command(mode: &str, root: &std::path::Path, timeout: Duration, limit: usize) -> GitCommand {
    let mut environment = vec![
        ("CIRCUS_PROCESS_HELPER".into(), mode.into()),
        (
            "CIRCUS_PROCESS_MARKER".into(),
            root.join("heartbeat").into_os_string(),
        ),
    ];
    if let Some(root) = std::env::var_os("SystemRoot") {
        environment.push(("SystemRoot".into(), root));
    }
    GitCommand {
        executable: std::env::current_exe().unwrap(),
        args: ["--exact", HELPER, "--nocapture"]
            .into_iter()
            .map(OsString::from)
            .collect(),
        cwd: root.to_owned(),
        environment,
        timeout,
        output_limit: limit,
    }
}

async fn await_heartbeat(root: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if tokio::fs::metadata(root.join("heartbeat")).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("descendant heartbeat");
}

async fn assert_stopped(root: &std::path::Path) {
    let before = tokio::fs::metadata(root.join("heartbeat"))
        .await
        .unwrap()
        .len();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        before,
        tokio::fs::metadata(root.join("heartbeat"))
            .await
            .unwrap()
            .len(),
        "descendant still executes after runner completion"
    );
}

#[tokio::test]
async fn stdout_and_stderr_share_one_budget() {
    let root = tempfile::tempdir().unwrap();
    let error = run_owned(
        command("output", root.path(), Duration::from_secs(10), 1200),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, ProcessError::OutputLimit { limit: 1200 }),
        "{error:?}"
    );
}

#[tokio::test]
async fn timeout_terminates_descendant() {
    let root = tempfile::tempdir().unwrap();
    let error = run_owned(
        command("wait", root.path(), Duration::from_secs(2), 4096),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ProcessError::Timeout), "{error:?}");
    assert_stopped(root.path()).await;
}

#[tokio::test]
async fn cancellation_terminates_descendant() {
    let root = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let runner = tokio::spawn(run_owned(
        command("wait", root.path(), Duration::from_secs(20), 4096),
        cancel.clone(),
    ));
    await_heartbeat(root.path()).await;
    cancel.cancel();
    let error = runner.await.unwrap().unwrap_err();
    assert!(matches!(error, ProcessError::Cancelled), "{error:?}");
    assert_stopped(root.path()).await;
}

#[tokio::test]
async fn normal_leader_exit_still_terminates_descendant() {
    let root = tempfile::tempdir().unwrap();
    let output = run_owned(
        command("exit", root.path(), Duration::from_secs(10), 4096),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(output.status.success());
    assert_stopped(root.path()).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn completion_observation_pins_leader_until_group_kill() {
    let root = tempfile::tempdir().unwrap();
    let spec = command("exit", root.path(), Duration::from_secs(10), 4096);
    let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child()).unwrap();
    let mut cmd = CommandWrap::with_new(spec.executable, |cmd| {
        cmd.args(spec.args)
            .env_clear()
            .envs(spec.environment)
            .current_dir(spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    });
    cmd.wrap(process_wrap::tokio::ProcessGroup::leader());
    let mut child = cmd.spawn().unwrap();
    let pid = child.id().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if observe_leader(pid).unwrap().is_some() {
                break;
            }
            signal.recv().await;
        }
    })
    .await
    .unwrap();
    // Repeated observation must leave the leader waitable: this is the identity
    // gate, not a probabilistic attempt to force actual kernel PID reuse.
    assert!(observe_leader(pid).unwrap().is_some());
    assert!(observe_leader(pid).unwrap().is_some());
    let mut may_signal = true;
    let observed = observe_leader(pid).unwrap();
    assert!(
        terminate(&mut *child, &mut may_signal, pid, observed)
            .await
            .unwrap()
            .success()
    );
    assert!(
        !may_signal,
        "cached PGID must never be signalled after reaping"
    );
    assert_stopped(root.path()).await;
}

#[tokio::test]
async fn dropping_caller_preserves_cleanup_owner() {
    let root = tempfile::tempdir().unwrap();
    let runner = tokio::spawn(run_owned(
        command("wait", root.path(), Duration::from_secs(20), 4096),
        CancellationToken::new(),
    ));
    await_heartbeat(root.path()).await;
    runner.abort();
    let _ = runner.await;
    // The caller task is gone, but the detached owner still receives the
    // cancellation guard and must terminate the actual descendant.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_stopped(root.path()).await;
}
