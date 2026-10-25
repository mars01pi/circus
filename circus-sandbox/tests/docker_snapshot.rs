//! Opt-in integration tests: real local Linux Docker Engine, never a mock.
use std::{process::Command, sync::Arc, time::Duration};

use circus_sandbox::{
    BackendConfig, CommandRequest, IoLimits, IsolationConfig, ResourceLimits, Sandbox,
    SandboxConfig, SandboxError, SandboxPhase, SnapshotBudget, SnapshotError, WorkspaceError,
};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
const ARCHIVE_CAP: u64 = 1024 * 1024;

fn backend() -> Result<BackendConfig> {
    Ok(BackendConfig::docker(
        std::env::var("CIRCUS_DOCKER_ENDPOINT")?,
        std::env::var("CIRCUS_DOCKER_IMAGE").unwrap_or_else(|_| "python:3.12-slim".into()),
        "runc",
    ))
}

fn host(backend: &BackendConfig) -> String {
    match backend {
        BackendConfig::Docker(docker) => {
            let endpoint = docker.endpoint();
            if endpoint.starts_with('/') {
                format!("unix://{endpoint}")
            } else {
                endpoint.to_string()
            }
        }
        BackendConfig::Containerd(_) => panic!("docker fixture received a containerd descriptor"),
        BackendConfig::Kubernetes(_) => panic!("docker fixture received a Kubernetes descriptor"),
    }
}

fn config(file_bytes: usize, workspace_bytes: u64) -> Result<SandboxConfig> {
    assert_eq!(
        std::env::var("CIRCUS_DOCKER_SANDBOX").as_deref(),
        Ok("1"),
        "set CIRCUS_DOCKER_SANDBOX=1 to opt into the real Engine tests"
    );
    Ok(SandboxConfig::new(
        ResourceLimits::new(64 * 1024 * 1024, 100, 32, workspace_bytes, file_bytes)?,
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)?,
        IoLimits::new(64 * 1024, Duration::from_secs(15))?,
    )?)
}

// The fixture owns exactly one container. Teardown is explicit and has no Drop
// fallback: `Sandbox::destroy` reports success only after confirmed absence,
// then the engine must also report the container gone. Unconfirmed removal
// fails the test instead of being ignored.
struct Fixture {
    sandbox: Sandbox,
    host: String,
    id: String,
}
impl Fixture {
    async fn new(file_bytes: usize, workspace_bytes: u64) -> Result<Self> {
        let backend = backend()?;
        let host = host(&backend);
        let sandbox = Sandbox::create(backend, config(file_bytes, workspace_bytes)?).await?;
        let id = sandbox
            .status()
            .await?
            .instance_id
            .expect("created instance ID");
        Ok(Self { sandbox, host, id })
    }
    async fn guest(&self, code: &str) -> Result<Vec<u8>> {
        let result = self
            .sandbox
            .exec(CommandRequest {
                argv: vec!["python3".into(), "-c".into(), code.into()],
                timeout: Duration::from_secs(10),
                output_limit_bytes: 16 * 1024,
            })
            .await?;
        assert_eq!(
            result.exit_code,
            0,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(result.stdout)
    }
    async fn destroy(&self) -> Result {
        self.sandbox.destroy().await?;
        assert_eq!(self.sandbox.status().await?.phase, SandboxPhase::Destroyed);
        let inspected = Command::new("docker")
            .args(["--host", &self.host, "inspect", &self.id])
            .output()?;
        if inspected.status.success() {
            return Err(format!("container {} still exists after destroy", self.id).into());
        }
        let stderr = String::from_utf8_lossy(&inspected.stderr);
        let absent = stderr.to_ascii_lowercase();
        // The CLI wording varies by version: "No such object" or "no such object".
        if !(absent.contains("no such") || absent.contains("not found")) {
            return Err(format!("could not confirm absence of {}: {stderr}", self.id).into());
        }
        Ok(())
    }
}
fn budget() -> SnapshotBudget {
    SnapshotBudget::new(64 * 1024, 128).unwrap()
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker Engine opt-in"]
async fn guest_bytes_and_empty_directories_survive_destroy_and_restore_not_processes() -> Result {
    let source = Fixture::new(64 * 1024, 8 * 1024 * 1024).await?;
    source.sandbox.start().await?;
    // A guest-only write cannot be recovered from the host staging tree or Docker commit.
    source.guest(r#"from pathlib import Path
import subprocess
Path('/workspace/empty/nested').mkdir(parents=True)
Path('/workspace/data.bin').write_bytes(bytes(range(256)) + b'\x00guest\xff')
subprocess.Popen(['python3','-c','import time; time.sleep(300)','circus-snapshot-process-token'], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
Path('/workspace/process-marker').write_text('copied bytes, not a continuing process')
"#).await?;
    let snapshot = source
        .sandbox
        .capture_workspace_snapshot(budget(), ARCHIVE_CAP)
        .await?;
    assert_eq!(source.sandbox.status().await?.phase, SandboxPhase::Running);
    assert_eq!(source.guest("print('resumed')").await?, b"resumed\n");
    source
        .sandbox
        .write_file("data.bin", b"changed after capture")
        .await?;
    source.destroy().await?;
    let destination = Fixture::new(64 * 1024, 8 * 1024 * 1024).await?;
    destination
        .sandbox
        .write_file("obsolete", b"must be replaced")
        .await?;
    destination
        .sandbox
        .restore_workspace_snapshot(Arc::clone(&snapshot))
        .await?;
    assert_eq!(
        destination.sandbox.status().await?.phase,
        SandboxPhase::Created
    );
    destination.sandbox.start().await?;
    let mut expected: Vec<u8> = (0..=255).collect();
    expected.extend_from_slice(b"\x00guest\xff");
    assert_eq!(
        destination
            .sandbox
            .read_file("data.bin")
            .await
            .map_err(|error| format!("read restored data.bin: {error:?}"))?,
        expected
    );
    assert_eq!(
        destination
            .sandbox
            .read_file("process-marker")
            .await
            .map_err(|error| format!("read restored process-marker: {error:?}"))?,
        b"copied bytes, not a continuing process"
    );
    assert_eq!(
        destination
            .guest(
                r#"from pathlib import Path
assert Path('/workspace/empty/nested').is_dir()
assert not any(Path('/workspace/empty/nested').iterdir())
assert not Path('/workspace/obsolete').exists()
needle = b'circus-snapshot-' + b'process-token'
assert not any(needle in p.read_bytes() for p in Path('/proc').glob('[0-9]*/cmdline') if p.exists())
print('files restored; no source process')"#
            )
            .await?,
        b"files restored; no source process\n"
    );
    destination.destroy().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker Engine opt-in"]
async fn user_paused_capture_stays_paused_and_restore_requires_created() -> Result {
    let source = Fixture::new(4096, 8 * 1024 * 1024).await?;
    source.sandbox.start().await?;
    source
        .guest("open('/workspace/guest','wb').write(b'paused bytes')")
        .await?;
    source.sandbox.pause().await?;
    let snapshot = source
        .sandbox
        .capture_workspace_snapshot(budget(), ARCHIVE_CAP)
        .await?;
    assert_eq!(source.sandbox.status().await?.phase, SandboxPhase::Paused);
    assert!(matches!(
        source
            .sandbox
            .restore_workspace_snapshot(Arc::clone(&snapshot))
            .await,
        Err(SandboxError::InvalidPhase {
            current: SandboxPhase::Paused,
            ..
        })
    ));
    source.sandbox.resume().await?;
    assert_eq!(source.sandbox.read_file("guest").await?, b"paused bytes");
    assert!(matches!(
        source
            .sandbox
            .restore_workspace_snapshot(Arc::clone(&snapshot))
            .await,
        Err(SandboxError::InvalidPhase {
            current: SandboxPhase::Running,
            ..
        })
    ));
    source.destroy().await?;
    assert!(matches!(
        source
            .sandbox
            .capture_workspace_snapshot(budget(), ARCHIVE_CAP)
            .await,
        Err(SandboxError::InvalidPhase {
            current: SandboxPhase::Destroyed,
            ..
        })
    ));
    assert!(matches!(
        source.sandbox.restore_workspace_snapshot(snapshot).await,
        Err(SandboxError::InvalidPhase {
            current: SandboxPhase::Destroyed,
            ..
        })
    ));
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker Engine opt-in"]
async fn created_capture_is_staging_and_restore_replaces_it() -> Result {
    let source = Fixture::new(4096, 8 * 1024 * 1024).await?;
    source.sandbox.create_dir_all("empty").await?;
    source
        .sandbox
        .write_file("staged", b"never started")
        .await?;
    let snapshot = source
        .sandbox
        .capture_workspace_snapshot(budget(), ARCHIVE_CAP)
        .await?;
    assert_eq!(source.sandbox.status().await?.phase, SandboxPhase::Created);
    source.destroy().await?;
    let destination = Fixture::new(4096, 8 * 1024 * 1024).await?;
    destination
        .sandbox
        .restore_workspace_snapshot(snapshot)
        .await?;
    assert_eq!(
        destination.sandbox.read_file("staged").await?,
        b"never started"
    );
    destination.sandbox.start().await?;
    destination
        .guest("from pathlib import Path; assert Path('/workspace/empty').is_dir()")
        .await?;
    destination.destroy().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker Engine opt-in"]
async fn payload_and_entry_failures_preserve_owned_source() -> Result {
    let source = Fixture::new(4096, 8 * 1024 * 1024).await?;
    source.sandbox.start().await?;
    source.guest("from pathlib import Path; Path('/workspace/a').write_bytes(b'x'*32); Path('/workspace/b').mkdir()").await?;
    let captured = source
        .sandbox
        .capture_workspace_snapshot(SnapshotBudget::new(31, 128)?, ARCHIVE_CAP)
        .await;
    assert!(
        matches!(
            captured,
            Err(SandboxError::Snapshot(SnapshotError::ByteLimit {
                limit: 31,
                ..
            }))
        ),
        "capture over the byte budget returned {captured:?}"
    );
    assert_eq!(source.sandbox.status().await?.phase, SandboxPhase::Running);
    assert!(matches!(
        source
            .sandbox
            .capture_workspace_snapshot(SnapshotBudget::new(4096, 1)?, ARCHIVE_CAP)
            .await,
        Err(SandboxError::Snapshot(SnapshotError::EntryLimit {
            limit: 1,
            ..
        }))
    ));
    assert_eq!(source.sandbox.status().await?.phase, SandboxPhase::Running);
    assert!(matches!(
        source.sandbox.capture_workspace_snapshot(budget(), 0).await,
        Err(SandboxError::Snapshot(SnapshotError::InvalidArchiveLimit))
    ));
    assert_eq!(source.sandbox.read_file("a").await?, vec![b'x'; 32]);
    source.sandbox.pause().await?;
    let captured = source
        .sandbox
        .capture_workspace_snapshot(SnapshotBudget::new(31, 128)?, ARCHIVE_CAP)
        .await;
    assert!(
        matches!(
            captured,
            Err(SandboxError::Snapshot(SnapshotError::ByteLimit {
                limit: 31,
                ..
            }))
        ),
        "capture over the byte budget returned {captured:?}"
    );
    assert_eq!(source.sandbox.status().await?.phase, SandboxPhase::Paused);
    source.sandbox.resume().await?;
    // Successful retry proves the failed captures left no poisoned staging/capture state.
    let snapshot = source
        .sandbox
        .capture_workspace_snapshot(budget(), ARCHIVE_CAP)
        .await?;
    source.destroy().await?;
    let destination = Fixture::new(4096, 8 * 1024 * 1024).await?;
    destination
        .sandbox
        .restore_workspace_snapshot(snapshot)
        .await?;
    destination.sandbox.start().await?;
    assert_eq!(destination.sandbox.read_file("a").await?, vec![b'x'; 32]);
    destination.destroy().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker Engine opt-in"]
async fn unsafe_guest_nodes_fail_closed_and_resume_for_cleanup() -> Result {
    for node in [
        "import os; os.symlink('/etc/passwd','/workspace/unsafe')",
        "import os; os.mkfifo('/workspace/unsafe')",
        "import os; open('/workspace/file','wb').write(b'x'); os.link('/workspace/file','/workspace/unsafe')",
    ] {
        let source = Fixture::new(4096, 8 * 1024 * 1024).await?;
        source.sandbox.start().await?;
        source.guest(node).await?;
        let captured = source
            .sandbox
            .capture_workspace_snapshot(budget(), ARCHIVE_CAP)
            .await;
        assert!(
            matches!(captured, Err(SandboxError::Snapshot(_))),
            "unsafe guest node {node} returned {captured:?}"
        );
        assert_eq!(source.sandbox.status().await?.phase, SandboxPhase::Running);
        assert_eq!(
            source.guest("print('owned and resumed')").await?,
            b"owned and resumed\n"
        );
        source.destroy().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker Engine opt-in"]
async fn restore_checks_target_file_and_workspace_bounds_before_replacing_staging() -> Result {
    let source = Fixture::new(4096, 8 * 1024 * 1024).await?;
    source.sandbox.start().await?;
    source
        .guest("open('/workspace/a','wb').write(b'a'*32); open('/workspace/b','wb').write(b'b'*32)")
        .await?;
    let snapshot = source
        .sandbox
        .capture_workspace_snapshot(budget(), ARCHIVE_CAP)
        .await?;
    source.destroy().await?;
    for (file_cap, total_cap) in [(31, 8 * 1024 * 1024), (63, 63)] {
        let destination = Fixture::new(file_cap, total_cap).await?;
        destination
            .sandbox
            .write_file("original", b"retained")
            .await?;
        let error = destination
            .sandbox
            .restore_workspace_snapshot(Arc::clone(&snapshot))
            .await;
        if file_cap == 31 {
            assert!(
                matches!(
                    error,
                    Err(SandboxError::Snapshot(SnapshotError::Workspace(
                        WorkspaceError::LimitExceeded { limit: 31, .. }
                    )))
                ),
                "restore with file cap 31 returned {error:?}"
            );
        } else {
            assert!(
                matches!(
                    error,
                    Err(SandboxError::Snapshot(SnapshotError::ByteLimit {
                        limit: 63,
                        ..
                    }))
                ),
                "restore with total cap 63 returned {error:?}"
            );
        }
        assert_eq!(
            destination.sandbox.status().await?.phase,
            SandboxPhase::Created
        );
        assert_eq!(
            destination.sandbox.read_file("original").await?,
            b"retained"
        );
        assert!(destination.sandbox.read_file("a").await.is_err());
        destination.sandbox.start().await?;
        assert_eq!(
            destination.sandbox.read_file("original").await?,
            b"retained"
        );
        destination.destroy().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker Engine opt-in"]
async fn archive_transport_failure_cleans_only_owned_container() -> Result {
    let source = Fixture::new(4096, 8 * 1024 * 1024).await?;
    let survivor = Fixture::new(4096, 8 * 1024 * 1024).await?;
    source.sandbox.start().await?;
    survivor.sandbox.start().await?;
    source
        .guest("open('/workspace/guest','wb').write(b'archive has payload')")
        .await?;
    assert!(matches!(
        source.sandbox.capture_workspace_snapshot(budget(), 1).await,
        Err(SandboxError::Backend { .. })
    ));
    assert_eq!(
        source.sandbox.status().await?.phase,
        SandboxPhase::Destroyed
    );
    assert_eq!(
        survivor.sandbox.status().await?.phase,
        SandboxPhase::Running
    );
    assert_eq!(
        survivor.guest("print('other owner survives')").await?,
        b"other owner survives\n"
    );
    source.destroy().await?;
    survivor.destroy().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker Engine opt-in"]
async fn guest_file_cap_and_nonportable_names_fail_before_snapshot_is_returned() -> Result {
    let source = Fixture::new(31, 8 * 1024 * 1024).await?;
    source.sandbox.start().await?;
    source
        .guest("open('/workspace/too-large','wb').write(b'x'*32)")
        .await?;
    let captured = source
        .sandbox
        .capture_workspace_snapshot(budget(), ARCHIVE_CAP)
        .await;
    assert!(
        matches!(
            captured,
            Err(SandboxError::Snapshot(SnapshotError::Workspace(
                WorkspaceError::LimitExceeded { limit: 31, .. }
            )))
        ),
        "capture over the file cap returned {captured:?}"
    );
    assert_eq!(source.sandbox.status().await?.phase, SandboxPhase::Running);
    source.guest("from pathlib import Path; Path('/workspace/too-large').unlink(); Path('/workspace/C:drive').write_bytes(b'x')").await?;
    assert!(matches!(
        source
            .sandbox
            .capture_workspace_snapshot(budget(), ARCHIVE_CAP)
            .await,
        Err(SandboxError::Snapshot(_))
    ));
    assert_eq!(source.sandbox.status().await?.phase, SandboxPhase::Running);
    source.guest("from pathlib import Path; Path('/workspace/C:drive').unlink(); Path('/workspace/safe').write_bytes(b'safe')").await?;
    let snapshot = source
        .sandbox
        .capture_workspace_snapshot(budget(), ARCHIVE_CAP)
        .await?;
    source.destroy().await?;
    let destination = Fixture::new(31, 8 * 1024 * 1024).await?;
    destination
        .sandbox
        .restore_workspace_snapshot(snapshot)
        .await?;
    destination.sandbox.start().await?;
    assert_eq!(destination.sandbox.read_file("safe").await?, b"safe");
    destination.destroy().await?;
    Ok(())
}
