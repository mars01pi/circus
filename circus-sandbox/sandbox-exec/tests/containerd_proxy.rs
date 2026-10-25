//! Opt-in real containerd acceptance for the standalone proxy.
//!
//! These tests are ignored by default. They only run when the operator sets
//! `CIRCUS_CONTAINERD=1` and points the fixture at an owned namespace. They
//! create and delete only `circus-sandbox-{recoveryId}` instances.

#![cfg(all(unix, feature = "containerd"))]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const WORKSPACE: &str = "/workspace";
const GUEST_UID: u32 = 65_534;

fn enabled() {
    assert_eq!(
        std::env::var("CIRCUS_CONTAINERD").as_deref(),
        Ok("1"),
        "set CIRCUS_CONTAINERD=1 to opt into the real containerd proxy test"
    );
}

fn socket() -> String {
    std::env::var("CIRCUS_CONTAINERD_SOCKET")
        .unwrap_or_else(|_| "/run/containerd/containerd.sock".to_string())
}

fn namespace() -> String {
    std::env::var("CIRCUS_CONTAINERD_NAMESPACE").unwrap_or_else(|_| "default".to_string())
}

fn image() -> String {
    std::env::var("CIRCUS_CONTAINERD_IMAGE").unwrap_or_else(|_| "python:3.12-slim".to_string())
}

fn architecture() -> String {
    std::env::var("CIRCUS_CONTAINERD_ARCH").unwrap_or_else(|_| match std::env::consts::ARCH {
        "x86_64" => "amd64".to_string(),
        "aarch64" => "arm64".to_string(),
        other => panic!("set CIRCUS_CONTAINERD_ARCH explicitly for host architecture {other}"),
    })
}

fn json_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "\\\\")
}

fn write_config(dir: &Path) -> PathBuf {
    let config = dir.join("config.json");
    let archive = dir.join("workspace.snap");
    let state = dir.join("state.json");
    let text = format!(
        r#"{{
            "version": 1,
            "backend": {{"kind": "containerd", "socket": "{}", "namespace": "{}", "image": "{}", "runtime": "runc", "snapshotter": "overlayfs"}},
            "imagePullPolicy": "local-only",
            "imagePlatform": {{"os": "linux", "architecture": "{}"}},
            "imagePrepareTimeoutMs": 120000,
            "limits": {{"memoryBytes": 134217728, "cpuMillis": 1000, "maxProcesses": 64, "workspaceBytes": 16777216, "maxFileBytes": 1048576}},
            "isolation": {{"workspacePath": "{WORKSPACE}", "guestUid": {GUEST_UID}, "guestGid": {GUEST_UID}}},
            "io": {{"outputLimitBytes": 1048576, "commandTimeoutMs": 30000, "fileTimeoutMs": 60000, "lifecycleTimeoutMs": 60000}},
            "workspace": {{"archivePath": "{}", "maxTotalBytes": 16777216, "maxEntries": 1024}},
            "statePath": "{}",
            "agent": {{"workingDir": null, "env": {{}}, "timeoutMs": 120000, "outputLimitBytes": 1048576}}
        }}"#,
        socket(),
        namespace(),
        image(),
        architecture(),
        json_path(&archive),
        json_path(&state),
    );
    std::fs::write(&config, text).unwrap();
    config
}

fn run(config: &Path, argv: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_circus-sandbox-exec"))
        .arg("--config")
        .arg(config)
        .arg("--")
        .args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

fn spawn_long_guest(config: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_circus-sandbox-exec"))
        .arg("--config")
        .arg(config)
        .arg("--")
        .arg("python3")
        .arg("-c")
        .arg("import time; print('ready', flush=True); time.sleep(600)")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn state_text(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("state.json")).unwrap()
}

fn recovery_id(state: &str) -> String {
    let marker = "\"recoveryId\":\"";
    let start = state.find(marker).expect("state has a recovery id") + marker.len();
    let end = state[start..].find('"').unwrap() + start;
    state[start..end].to_string()
}

fn ctr(args: &[&str]) -> Output {
    let namespace = namespace();
    Command::new("ctr")
        .args(["-n", namespace.as_str()])
        .args(args)
        .output()
        .expect("ctr must be installed for the real containerd suite")
}

fn assert_absent(recovery: &str) {
    let name = format!("circus-sandbox-{recovery}");
    let listed = ctr(&["containers", "list", "-q"]);
    assert!(
        listed.status.success(),
        "ctr containers list failed: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        !listed.lines().any(|line| line.trim() == name),
        "owned container {name} is still present"
    );
    let snapshots = ctr(&["snapshots", "--snapshotter", "overlayfs", "list"]);
    assert!(
        snapshots.status.success(),
        "ctr snapshots list failed: {}",
        String::from_utf8_lossy(&snapshots.stderr)
    );
    let snapshots = String::from_utf8_lossy(&snapshots.stdout);
    assert!(
        !snapshots
            .lines()
            .skip(1)
            .any(|line| line.split_whitespace().next() == Some(name.as_str())),
        "owned snapshot {name} is still present"
    );
}

fn wait_until_running(child: &mut Child, state_path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            panic!("proxy exited before the guest became reachable: {status}");
        }
        if let Ok(state) = std::fs::read_to_string(state_path)
            && state.contains("\"phase\":\"running\"")
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("sandbox did not reach running state within 120 seconds");
}

#[test]
#[ignore = "requires CIRCUS_CONTAINERD=1 and an owned containerd namespace"]
fn archive_round_trip_preserves_workspace_and_confirms_cleanup() {
    enabled();
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());

    let first = run(
        &config,
        &[
            "python3",
            "-c",
            "import sys; data=sys.stdin.buffer.read(); open('/workspace/note.txt','wb').write(b'hello'); sys.stdout.buffer.write(b'out:'+data); sys.stderr.buffer.write(b'err')",
        ],
        b"ping",
    );
    assert!(
        first.status.success(),
        "first proxy run failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(first.stdout, b"out:ping");
    assert_eq!(first.stderr, b"err");
    assert!(dir.path().join("workspace.snap").is_file());
    let state = state_text(dir.path());
    assert!(state.contains("\"phase\":\"closed\""));
    let recovery = recovery_id(&state);
    assert_absent(&recovery);

    let second = run(
        &config,
        &[
            "python3",
            "-c",
            "import sys; sys.stdout.buffer.write(open('/workspace/note.txt','rb').read())",
        ],
        b"",
    );
    assert!(
        second.status.success(),
        "second proxy run failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(second.stdout, b"hello");
    assert!(second.stderr.is_empty());
    assert!(state_text(dir.path()).contains("\"phase\":\"closed\""));
    assert_absent(&recovery_id(&state_text(dir.path())));
}

#[test]
#[ignore = "requires CIRCUS_CONTAINERD=1 and an owned containerd namespace"]
fn killed_proxy_orphan_is_recovered_before_a_new_create() {
    enabled();
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let state_path = dir.path().join("state.json");

    let mut child = spawn_long_guest(&config);
    wait_until_running(&mut child, &state_path);
    let recovery = recovery_id(&state_text(dir.path()));
    let name = format!("circus-sandbox-{recovery}");
    let listed = ctr(&["containers", "list", "-q"]);
    assert!(listed.status.success());
    assert!(
        String::from_utf8_lossy(&listed.stdout)
            .lines()
            .any(|line| line.trim() == name),
        "expected owned container {name} before proxy death"
    );

    child.kill().unwrap();
    let _ = child.wait();

    let recovered = run(&config, &["python3", "-c", "print('recovered')"], b"");
    assert!(
        recovered.status.success(),
        "recovery run failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(recovered.stdout, b"recovered\n");
    assert!(state_text(dir.path()).contains("\"phase\":\"closed\""));
    assert_absent(&recovery);
}
