use std::time::Duration;

use circus_sandbox::runtime::ResourceLimits;
use circus_sandbox::{
    BackendConfig, BackendConfigError, IoLimits, IsolationConfig, Sandbox, SandboxConfig,
    SandboxError,
};

fn config(pids: u32) -> Result<SandboxConfig, Box<dyn std::error::Error>> {
    let isolation =
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 1000, 1000)?;
    let limits = ResourceLimits::new(64 * 1024 * 1024, 250, pids, 16 * 1024 * 1024, 1024 * 1024)?;
    Ok(SandboxConfig::new(
        limits,
        isolation,
        IoLimits::new(1024 * 1024, Duration::from_secs(5))?,
    )?)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let valid = config(3)?;
    println!(
        "valid workspace={} output_limit={} file_timeout={:?}",
        valid.isolation().workspace_path(),
        valid.output_limit_bytes(),
        valid.file_timeout()
    );

    let backend = BackendConfig::docker("/var/run/docker.sock", "alpine:3.20", "runc");
    match &backend {
        BackendConfig::Docker(docker) => println!(
            "backend endpoint={} image={} runtime={}",
            docker.endpoint(),
            docker.image(),
            docker.runtime()
        ),
        BackendConfig::Containerd(containerd) => println!(
            "backend socket={} namespace={} snapshotter={} image={} runtime={}",
            containerd.socket(),
            containerd.namespace(),
            containerd.snapshotter(),
            containerd.image(),
            containerd.runtime()
        ),
        BackendConfig::Kubernetes(kubernetes) => println!(
            "backend kubeconfig={} namespace={} image={}",
            kubernetes.kubeconfig(),
            kubernetes.namespace(),
            kubernetes.image()
        ),
    }

    let error = match Sandbox::create(
        BackendConfig::docker("tcp://127.0.0.1:2375", "alpine:3.20", "runc"),
        valid,
    )
    .await
    {
        Err(error) => error,
        Ok(_) => return Err("remote endpoint was not rejected".into()),
    };
    match error {
        SandboxError::BackendConfig(BackendConfigError::UnsupportedEndpoint { .. }) => {
            println!("typed invalid backend descriptor rejected before allocation");
        }
        other => return Err(format!("unexpected error: {other}").into()),
    }

    let error = match Sandbox::create(
        BackendConfig::docker("/var/run/docker.sock", "alpine:3.20", "runc"),
        config(2)?,
    )
    .await
    {
        Err(error) => error,
        Ok(_) => return Err("PID floor was not enforced".into()),
    };
    match error {
        SandboxError::BackendConfig(BackendConfigError::SupervisorNeedsPid { pids }) => {
            println!("typed Docker PID floor rejection for pids={pids}");
        }
        other => return Err(format!("unexpected error: {other}").into()),
    }
    Ok(())
}
