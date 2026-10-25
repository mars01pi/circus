use std::time::Duration;

use circus_sandbox::{
    BackendConfig, CommandRequest, IoLimits, IsolationConfig, ResourceLimits, Sandbox,
    SandboxConfig,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let backend = BackendConfig::docker(
        std::env::var("CIRCUS_DOCKER_ENDPOINT")?,
        std::env::var("CIRCUS_DOCKER_IMAGE").unwrap_or_else(|_| "python:3.12-slim".to_string()),
        "runc",
    );
    let limits = ResourceLimits::new(64 * 1024 * 1024, 100, 32, 8 * 1024 * 1024, 1024 * 1024)?;
    let isolation =
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)?;
    let config = SandboxConfig::new(
        limits,
        isolation,
        IoLimits::new(64 * 1024, Duration::from_secs(10))?,
    )?;
    let sandbox = Sandbox::create(backend, config).await?;
    sandbox
        .write_file("hello.txt", b"hello from circus")
        .await?;
    sandbox.start().await?;
    let output = sandbox
        .exec(CommandRequest {
            argv: vec!["/bin/cat".into(), "/workspace/hello.txt".into()],
            timeout: Duration::from_secs(10),
            output_limit_bytes: 4096,
        })
        .await?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    assert_eq!(sandbox.read_file("hello.txt").await?, b"hello from circus");
    sandbox.pause().await?;
    sandbox.resume().await?;
    sandbox.destroy().await?;
    Ok(())
}
