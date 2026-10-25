use std::collections::BTreeMap;
use std::time::Duration;

use circus_sandbox::{
    BackendConfig, IoLimits, IsolationConfig, ProcessExit, ProcessOutput, ProcessRequest,
    ResourceLimits, Sandbox, SandboxConfig,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let backend = BackendConfig::docker(
        std::env::var("CIRCUS_DOCKER_ENDPOINT")?,
        std::env::var("CIRCUS_DOCKER_IMAGE").unwrap_or_else(|_| "python:3.12-slim".into()),
        "runc",
    );
    let config = SandboxConfig::new(
        ResourceLimits::new(64 * 1024 * 1024, 1000, 32, 8 * 1024 * 1024, 1024 * 1024)?,
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)?,
        IoLimits::new(64 * 1024, Duration::from_secs(10))?,
    )?;
    let sandbox = Sandbox::create(backend, config).await?;
    sandbox.start().await?;
    let session = sandbox
        .spawn_process(ProcessRequest {
            argv: vec![
                "python3".into(),
                "-u".into(),
                "-c".into(),
                "import sys\nfor line in sys.stdin:\n print('reply: ' + line.strip())".into(),
            ],
            working_dir: None,
            env: BTreeMap::new(),
            timeout: Duration::from_secs(20),
            output_limit_bytes: 4096,
        })
        .await?;
    let send = async {
        session.write_stdin(b"first message\n").await?;
        session.write_stdin(b"second message\n").await?;
        session.close_stdin().await
    };
    let receive = async {
        while let Some(chunk) = session.next_output().await {
            match chunk {
                ProcessOutput::Stdout(bytes) => print!("{}", String::from_utf8_lossy(&bytes)),
                ProcessOutput::Stderr(bytes) => eprint!("{}", String::from_utf8_lossy(&bytes)),
            }
        }
    };
    let (input, ()) = tokio::join!(send, receive);
    let result = session.wait().await;
    // Observe cleanup even if input or the process failed.
    sandbox.destroy().await?;
    input?;
    match result? {
        ProcessExit::Exited { exit_code: 0 } => Ok(()),
        other => Err(format!("interactive process failed: {other:?}").into()),
    }
}
