use std::time::Duration;

use circus_sandbox::runtime::ResourceLimits;
use circus_sandbox::{
    BackendConfig, BackendConfigError, ConfigError, IoLimits, IsolationConfig, Sandbox,
    SandboxConfig, SandboxError,
};

fn limits(pids: u32) -> ResourceLimits {
    ResourceLimits::new(64 * 1024, 100, pids, 32 * 1024, 1024).unwrap()
}

fn isolation() -> IsolationConfig {
    IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 1000, 1000).unwrap()
}

fn config(pids: u32) -> SandboxConfig {
    SandboxConfig::new(
        limits(pids),
        isolation(),
        IoLimits::new(4096, Duration::from_secs(2)).unwrap(),
    )
    .unwrap()
}

#[test]
fn rejects_insecure_flags_and_io_bounds() {
    assert!(matches!(
        IsolationConfig::linux_enforced(false, true, true, true, "/workspace", 1000, 1000),
        Err(ConfigError::IsolationNotEnforced)
    ));
    assert!(IoLimits::new(0, Duration::from_secs(1)).is_err());
    assert!(IoLimits::new(1, Duration::ZERO).is_err());
}

#[test]
fn rejects_platform_maximum_output_limit() {
    assert_eq!(
        IoLimits::new(usize::MAX, Duration::from_secs(1)),
        Err(ConfigError::InvalidOutputLimit {
            output_limit_bytes: usize::MAX
        })
    );
}

#[test]
fn rejects_relative_isolation_workspace_path() {
    assert!(matches!(
        IsolationConfig::linux_enforced(true, true, true, true, "workspace", 1000, 1000),
        Err(ConfigError::InvalidWorkspacePath)
    ));
}

#[tokio::test]
async fn create_rejects_remote_endpoints_before_connecting() {
    let error = Sandbox::create(
        BackendConfig::docker("tcp://127.0.0.1:2375", "image:tag", "runc"),
        config(3),
    )
    .await
    .expect_err("remote endpoints must be rejected");
    assert!(matches!(
        error,
        SandboxError::BackendConfig(BackendConfigError::UnsupportedEndpoint { .. })
    ));
}

#[tokio::test]
async fn create_rejects_unknown_runtime_before_connecting() {
    let error = Sandbox::create(
        BackendConfig::docker("/var/run/docker.sock", "image", "runsc"),
        config(3),
    )
    .await
    .expect_err("unsupported runtimes must be rejected");
    assert!(matches!(
        error,
        SandboxError::BackendConfig(BackendConfigError::UnsupportedRuntime { .. })
    ));
}

#[tokio::test]
async fn create_rejects_docker_supervisor_pid_floor_before_connecting() {
    let error = Sandbox::create(
        BackendConfig::docker("/var/run/docker.sock", "image", "runc"),
        config(2),
    )
    .await
    .expect_err("the Docker PID floor must be enforced");
    assert!(matches!(
        error,
        SandboxError::BackendConfig(BackendConfigError::SupervisorNeedsPid { pids: 2 })
    ));
}

#[test]
fn accepts_valid_configuration_and_exposes_typed_values() {
    let config = config(3);
    assert_eq!(config.limits(), limits(3));
    assert_eq!(config.isolation().workspace_path(), "/workspace");
    assert_eq!(config.output_limit_bytes(), 4096);
    assert_eq!(config.file_timeout(), Duration::from_secs(2));

    let backend = BackendConfig::docker("unix:///var/run/docker.sock", "alpine:3.20", "runc");
    match backend {
        BackendConfig::Docker(docker) => {
            assert_eq!(docker.endpoint(), "unix:///var/run/docker.sock");
            assert_eq!(docker.image(), "alpine:3.20");
            assert_eq!(docker.runtime(), "runc");
        }
        BackendConfig::Containerd(_) => panic!("expected a Docker descriptor"),
        BackendConfig::Kubernetes(_) => panic!("expected a Docker descriptor"),
    }
}

#[cfg(all(unix, feature = "containerd"))]
#[tokio::test]
async fn create_rejects_unsupported_containerd_snapshotter_before_connecting() {
    let error = Sandbox::create(
        BackendConfig::containerd(
            "/run/containerd/containerd.sock",
            "circus",
            "python:3.12-slim",
            "runc",
            "native",
        ),
        config(3),
    )
    .await
    .expect_err("unsupported snapshotters must be rejected");
    assert!(matches!(
        error,
        SandboxError::BackendConfig(BackendConfigError::UnsupportedSnapshotter { .. })
    ));
}

#[cfg(feature = "kubernetes")]
#[tokio::test]
async fn create_rejects_relative_kubeconfig_before_connecting() {
    let error = Sandbox::create(
        BackendConfig::kubernetes("relative.yaml", "sandboxes", "python:3.12-slim"),
        config(3),
    )
    .await
    .expect_err("relative kubeconfig paths must be rejected");
    assert!(matches!(
        error,
        SandboxError::BackendConfig(BackendConfigError::InvalidKubeconfigPath { .. })
    ));
}
