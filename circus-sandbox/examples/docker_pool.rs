use std::sync::Arc;
use std::time::{Duration, Instant};

use circus_sandbox::{
    BackendConfig, CommandRequest, IoLimits, IsolationConfig, PoolConfig, PoolError,
    ResourceLimits, Sandbox, SandboxConfig, SandboxPool, SnapshotBudget, Workspace,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("CIRCUS_DOCKER_SANDBOX").as_deref() != Ok("1") {
        return Err("set CIRCUS_DOCKER_SANDBOX=1 to run the real Docker example".into());
    }
    let backend = BackendConfig::docker(
        std::env::var("CIRCUS_DOCKER_ENDPOINT")?,
        std::env::var("CIRCUS_DOCKER_IMAGE").unwrap_or_else(|_| "python:3.12-slim".into()),
        "runc",
    );
    let config = SandboxConfig::new(
        ResourceLimits::new(64 * 1024 * 1024, 100, 32, 8 * 1024 * 1024, 1024 * 1024)?,
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)?,
        IoLimits::new(64 * 1024, Duration::from_secs(10))?,
    )?;
    // Only explicitly authorized shared input belongs in the baseline, never tenant credentials.
    let mut source = Workspace::new(1024 * 1024)?;
    let program = b"print('executed baseline bytes')\n";
    source.write_file("hello.py", program)?;
    source.create_dir_all("empty")?;
    let baseline = Arc::new(source.capture_code_snapshot(SnapshotBudget::new(1024 * 1024, 16)?)?);
    source.cleanup()?;

    let cold_start = Instant::now();
    let cold = Sandbox::create(backend.clone(), config.clone()).await?;
    let cold_result = async {
        cold.restore_workspace_snapshot(Arc::clone(&baseline))
            .await?;
        cold.start().await?;
        Ok::<_, circus_sandbox::SandboxError>(cold_start.elapsed())
    }
    .await;
    // Explicit cleanup is attempted even if restoration or startup failed.
    let cold_cleanup = cold.destroy().await;
    let cold_elapsed = cold_result?;
    cold_cleanup?;

    let warmup_start = Instant::now();
    let pool = match SandboxPool::create(PoolConfig::new(
        backend,
        config,
        1,
        Some(baseline),
        Duration::from_secs(120),
    )?)
    .await
    {
        Ok(pool) => pool,
        Err(PoolError::CleanupRequired { pool, source, .. }) => {
            eprintln!("pool preparation failed; retrying retained cleanup: {source}");
            pool.shutdown().await?;
            return Err(source.into());
        }
        Err(error) => return Err(error.into()),
    };
    let warmup_elapsed = warmup_start.elapsed();
    let result = async {
        let acquire_start = Instant::now();
        let lease = pool.acquire(Duration::from_secs(30)).await?;
        let acquire_elapsed = acquire_start.elapsed();
        let execution = async {
            assert_eq!(lease.sandbox().read_file("hello.py").await?, program);
            let output = lease
                .sandbox()
                .exec(CommandRequest {
                    argv: vec!["python3".into(), "/workspace/hello.py".into()],
                    timeout: Duration::from_secs(10),
                    output_limit_bytes: 4096,
                })
                .await?;
            assert_eq!(output.exit_code, 0);
            assert_eq!(output.stdout, b"executed baseline bytes\n");
            print!("{}", String::from_utf8_lossy(&output.stdout));
            Ok::<(), Box<dyn std::error::Error>>(())
        }
        .await;
        let release = lease.release().await;
        execution?;
        release?;
        Ok::<_, Box<dyn std::error::Error>>(acquire_elapsed)
    }
    .await;
    // Shutdown revokes outstanding leases; completion, not Drop, confirms cleanup.
    let shutdown = pool.shutdown().await;
    let acquire_elapsed = result?;
    shutdown?;
    println!("observed cold create + restore + start: {cold_elapsed:?}");
    println!("observed pool warmup (paid before acquisition): {warmup_elapsed:?}");
    println!("observed prestarted lease acquisition: {acquire_elapsed:?}");
    println!("These are individual observations, not a guaranteed speedup or process checkpoint.");
    Ok(())
}
