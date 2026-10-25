//! Real Docker workspace-file copy; not a process/memory checkpoint or Docker commit.
use std::time::Duration;

use circus_sandbox::{
    BackendConfig, CommandRequest, IoLimits, IsolationConfig, ResourceLimits, Sandbox,
    SandboxConfig, SnapshotBudget,
};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn backend() -> Result<BackendConfig> {
    Ok(BackendConfig::docker(
        std::env::var("CIRCUS_DOCKER_ENDPOINT")?,
        std::env::var("CIRCUS_DOCKER_IMAGE").unwrap_or_else(|_| "python:3.12-slim".into()),
        "runc",
    ))
}

fn config() -> Result<SandboxConfig> {
    Ok(SandboxConfig::new(
        ResourceLimits::new(64 * 1024 * 1024, 100, 32, 8 * 1024 * 1024, 64 * 1024)?,
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)?,
        IoLimits::new(64 * 1024, Duration::from_secs(15))?,
    )?)
}

#[tokio::main]
async fn main() -> Result {
    let backend = backend()?;
    let source = Sandbox::create(backend.clone(), config()?).await?;
    let capture: Result<_> = async {
        source.start().await?;
        let write = source.exec(CommandRequest {
            argv: vec!["python3".into(), "-c".into(),
                "from pathlib import Path; Path('/workspace/empty').mkdir(); Path('/workspace/guest.txt').write_bytes(b'written inside the running guest\\n')".into()],
            timeout: Duration::from_secs(10),
            output_limit_bytes: 4096,
        }).await?;
        if write.exit_code != 0 {
            return Err(format!("guest write failed: {}", String::from_utf8_lossy(&write.stderr)).into());
        }
        let snapshot = source.capture_workspace_snapshot(
            SnapshotBudget::new(64 * 1024, 128)?, 1024 * 1024,
        ).await?;
        println!("Source after capture: {:?}", source.status().await?.phase);
        Ok(snapshot)
    }.await;
    // Explicitly destroy even when capture failed; cleanup errors are propagated.
    source.destroy().await?;
    let snapshot = capture?;
    println!("Source destroyed; only workspace files/directories were copied, not processes.");

    let destination = Sandbox::create(backend, config()?).await?;
    let restored: Result = async {
        destination.restore_workspace_snapshot(snapshot).await?;
        destination.start().await?;
        let observed = destination.read_file("guest.txt").await?;
        println!("Observed restored bytes: {:?}", observed);
        print!("Observed restored text: {}", String::from_utf8_lossy(&observed));
        if observed != b"written inside the running guest\n" {
            return Err("restored bytes differ from the guest write".into());
        }
        let read = destination.exec(CommandRequest {
            argv: vec!["python3".into(), "-c".into(),
                "from pathlib import Path; p=Path('/workspace/empty'); print('empty directory restored:', p.is_dir() and not any(p.iterdir()))".into()],
            timeout: Duration::from_secs(10),
            output_limit_bytes: 4096,
        }).await?;
        if read.exit_code != 0 {
            return Err(format!("guest read failed: {}", String::from_utf8_lossy(&read.stderr)).into());
        }
        print!("{}", String::from_utf8_lossy(&read.stdout));
        Ok(())
    }.await;
    destination.destroy().await?;
    restored
}
