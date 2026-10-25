//! Opt-in integration tests for bounded staging transfer on a real local
//! Linux Docker Engine. Never a mock.
use std::process::Command;
use std::time::Duration;

use circus_sandbox::{
    BackendConfig, CommandRequest, IoLimits, IsolationConfig, ResourceLimits, Sandbox,
    SandboxConfig, SandboxError, SandboxPhase, StagingError,
};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

// Temporary OCI exec tasks are charged to the container pid cgroup.
const TEST_PIDS: u32 = 32;
const WORKSPACE_BYTES: u64 = 256 * 1024;
const MAX_FILE_BYTES: usize = 128 * 1024;

fn enabled() {
    assert_eq!(
        std::env::var("CIRCUS_DOCKER_SANDBOX").as_deref(),
        Ok("1"),
        "set CIRCUS_DOCKER_SANDBOX=1 to opt into the real backend test"
    );
}

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

fn config() -> Result<SandboxConfig> {
    Ok(SandboxConfig::new(
        ResourceLimits::new(
            64 * 1024 * 1024,
            100,
            TEST_PIDS,
            WORKSPACE_BYTES,
            MAX_FILE_BYTES,
        )?,
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)?,
        IoLimits::new(64 * 1024, Duration::from_secs(10))?,
    )?)
}

// The fixture owns exactly one container. Teardown is explicit and has no Drop
// fallback: `Sandbox::destroy` reports success only after confirmed absence,
// then the engine must also report the container gone. Unconfirmed removal
// fails the test instead of being ignored.
struct Fixture {
    sandbox: Option<Sandbox>,
    host: String,
    id: String,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let backend = backend()?;
        let host = host(&backend);
        let sandbox = Sandbox::create(backend, config()?).await?;
        let id = sandbox
            .status()
            .await?
            .instance_id
            .expect("created instance ID");
        Ok(Self {
            sandbox: Some(sandbox),
            host,
            id,
        })
    }

    fn sandbox(&self) -> &Sandbox {
        self.sandbox.as_ref().expect("fixture is alive")
    }
}

async fn teardown(f: &mut Fixture) -> Result {
    if let Some(sandbox) = f.sandbox.take() {
        sandbox.destroy().await?;
    }
    let inspected = Command::new("docker")
        .args(["--host", &f.host, "inspect", &f.id])
        .output()?;
    if inspected.status.success() {
        return Err(format!("teardown did not remove container {}", f.id).into());
    }
    let stderr = String::from_utf8_lossy(&inspected.stderr);
    let absent = stderr.to_ascii_lowercase();
    // The CLI wording varies by version: "No such object" or "no such object".
    if !(absent.contains("no such") || absent.contains("not found")) {
        return Err(format!("teardown could not confirm absence of {}: {stderr}", f.id).into());
    }
    Ok(())
}

fn guest_probe() -> CommandRequest {
    CommandRequest {
        argv: vec![
            "python3".into(),
            "-I".into(),
            "-c".into(),
            r###"
from pathlib import Path
root = Path('/workspace')
paths = sorted(str(p.relative_to(root)).replace('\\', '/') for p in root.rglob('*'))
print('\n'.join(paths))
print((root / 'nested/a.txt').read_bytes() == b'A' * 1000)
print((root / 'nested/inner/b.txt').read_bytes() == b'B' * 500)
print(len((root / 'near.bin').read_bytes()))
print((root / 'empty').is_dir())
print(sum(len(p.read_bytes()) for p in root.rglob('*') if p.is_file()))
"###
            .into(),
        ],
        timeout: Duration::from_secs(10),
        output_limit_bytes: 16 * 1024,
    }
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn docker_staging_tree_over_limit_is_typed_and_retry_streams_guest_bytes() -> Result {
    enabled();
    let mut f = Fixture::new().await?;
    let near_limit = vec![b'N'; MAX_FILE_BYTES];
    let over = vec![b'O'; 120 * 1024];
    f.sandbox().create_dir_all("nested/inner").await?;
    f.sandbox().create_dir_all("empty").await?;
    f.sandbox()
        .write_file("nested/a.txt", &vec![b'A'; 1000])
        .await?;
    f.sandbox()
        .write_file("nested/inner/b.txt", &vec![b'B'; 500])
        .await?;
    // Equality with the per-file bound is allowed.
    f.sandbox().write_file("near.bin", &near_limit).await?;
    // Two more in-bound files push the tree over the workspace bound.
    f.sandbox().write_file("over1.bin", &over).await?;
    f.sandbox().write_file("over2.bin", &over).await?;

    let error = f
        .sandbox()
        .start()
        .await
        .expect_err("aggregate staging bound must reject start");
    match &error {
        SandboxError::Staging(StagingError::LimitExceeded { limit, actual }) => {
            assert_eq!(*limit, WORKSPACE_BYTES);
            assert!(*actual > WORKSPACE_BYTES);
        }
        other => panic!("unexpected staging error: {other:?}"),
    }
    let status = f.sandbox().status().await?;
    assert_eq!(status.phase, SandboxPhase::Created);
    assert_eq!(status.instance_id.as_deref(), Some(f.id.as_str()));
    assert_eq!(
        f.sandbox().read_file("near.bin").await?,
        near_limit,
        "staging is retained after a rejected start"
    );

    f.sandbox().remove_file("over1.bin").await?;
    f.sandbox().remove_file("over2.bin").await?;
    f.sandbox().start().await?;
    assert_eq!(f.sandbox().status().await?.phase, SandboxPhase::Running);

    let observed = f.sandbox().exec(guest_probe()).await?;
    assert!(
        observed.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&observed.stderr)
    );
    assert_eq!(observed.exit_code, 0);
    let text = String::from_utf8(observed.stdout)?;
    let expected: Vec<String> = vec![
        "empty".into(),
        "near.bin".into(),
        "nested".into(),
        "nested/a.txt".into(),
        "nested/inner".into(),
        "nested/inner/b.txt".into(),
        "True".into(),
        "True".into(),
        MAX_FILE_BYTES.to_string(),
        "True".into(),
        (MAX_FILE_BYTES + 1000 + 500).to_string(),
    ];
    assert_eq!(
        text.lines().map(str::to_string).collect::<Vec<_>>(),
        expected
    );

    // A streamed file larger than one copy chunk downloads intact.
    let downloaded = f.sandbox().read_file("near.bin").await?;
    assert_eq!(downloaded.len(), MAX_FILE_BYTES);
    assert!(downloaded.iter().all(|byte| *byte == b'N'));

    teardown(&mut f).await?;
    Ok(())
}
