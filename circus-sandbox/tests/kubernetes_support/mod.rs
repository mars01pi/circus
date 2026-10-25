//! Shared fixtures for the opt-in real Kubernetes suites.
//!
//! Every suite is ignored unless `CIRCUS_KUBERNETES=1`. Absence is asserted
//! with the real `kubectl` CLI against the same namespace, never with a mock.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::process::{Command, Output};
use std::time::Duration;

use circus_sandbox::{
    BackendConfig, BackendConfigError, CommandRequest, IoLimits, IsolationConfig, ResourceLimits,
    Sandbox, SandboxConfig, SandboxError, SnapshotBudget,
};

pub type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

pub fn enabled() {
    assert_eq!(
        std::env::var("CIRCUS_KUBERNETES").as_deref(),
        Ok("1"),
        "set CIRCUS_KUBERNETES=1 to opt into the real Kubernetes suites"
    );
}

pub fn kubeconfig() -> String {
    std::env::var("CIRCUS_KUBERNETES_KUBECONFIG")
        .unwrap_or_else(|_| "/etc/rancher/k3s/k3s.yaml".to_string())
}

pub fn namespace() -> String {
    std::env::var("CIRCUS_KUBERNETES_NAMESPACE").unwrap_or_else(|_| "default".to_string())
}

pub fn image() -> String {
    std::env::var("CIRCUS_KUBERNETES_IMAGE")
        .unwrap_or_else(|_| "docker.io/library/python:3.12-slim".to_string())
}

/// A well-formed reference that no registry can resolve. The API server admits
/// it; the kubelet fails the pull, which startup must report.
pub fn absent_image() -> String {
    std::env::var("CIRCUS_KUBERNETES_ABSENT_IMAGE")
        .unwrap_or_else(|_| "registry.invalid/circus/absent:never".to_string())
}

pub fn backend() -> BackendConfig {
    BackendConfig::kubernetes(kubeconfig(), namespace(), image())
}

pub fn backend_in(namespace: &str, image: &str) -> BackendConfig {
    BackendConfig::kubernetes(kubeconfig(), namespace, image)
}

pub fn config() -> Result<SandboxConfig> {
    config_with(1024 * 1024, 8 * 1024 * 1024)
}

/// Neutral configuration with explicit per-file and workspace bounds.
pub fn config_with(max_file_bytes: usize, workspace_bytes: u64) -> Result<SandboxConfig> {
    Ok(SandboxConfig::new(
        ResourceLimits::new(128 * 1024 * 1024, 1000, 64, workspace_bytes, max_file_bytes)?,
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 1000, 1000)?,
        IoLimits::new(4 * 1024 * 1024, Duration::from_secs(20))?,
    )?)
}

/// Generous capture budget for the small fixtures used by these suites.
pub fn budget() -> Result<SnapshotBudget> {
    Ok(SnapshotBudget::new(64 * 1024, 16)?)
}

pub async fn create() -> Result<Sandbox> {
    Ok(Sandbox::create(backend(), config()?).await?)
}

pub fn python(argv: &[&str]) -> CommandRequest {
    CommandRequest {
        argv: std::iter::once("python3".to_string())
            .chain(argv.iter().map(|value| (*value).to_string()))
            .collect(),
        timeout: Duration::from_secs(15),
        output_limit_bytes: 1024 * 1024,
    }
}

/// Runs `kubectl` against the configured kubeconfig and namespace.
pub fn kubectl<S: AsRef<OsStr>>(args: &[S]) -> Result<Output> {
    Ok(Command::new("kubectl")
        .arg("--kubeconfig")
        .arg(kubeconfig())
        .arg("--namespace")
        .arg(namespace())
        .args(args)
        .output()?)
}

pub fn pod_exists(name: &str) -> Result<bool> {
    let output = kubectl(&["get", "pod", name, "-o", "name"])?;
    if output.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("NotFound") || stderr.contains("not found") {
        return Ok(false);
    }
    Err(format!("kubectl get pod {name} failed: {stderr}").into())
}

/// Confirms with `kubectl` that the Pod is absent.
pub fn assert_absent(name: &str) -> Result {
    if pod_exists(name)? {
        return Err(format!("Pod {name} is still listed by kubectl").into());
    }
    Ok(())
}

/// Polls the API through `kubectl` until the Pod is confirmed absent.
///
/// Deletion is asynchronous even with a zero grace period, so this waits for
/// the real API instead of assuming the delete response is completion.
pub async fn eventually_absent(name: &str) -> Result {
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            if assert_absent(name).is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| format!("Pod {name} did not disappear"))?;
    assert_absent(name)
}

/// Destroys a sandbox and confirms the Pod is gone with `kubectl`.
pub async fn destroy_and_assert(sandbox: &Sandbox, name: &str) -> Result {
    sandbox.destroy().await?;
    assert_absent(name)
}

/// Number of Pods owned by this crate in the configured namespace.
pub fn managed_pod_count() -> Result<usize> {
    let output = kubectl(&[
        "get",
        "pods",
        "-l",
        "app.kubernetes.io/managed-by=circus-sandbox",
        "-o",
        "name",
        "--no-headers",
    ])?;
    if !output.status.success() {
        return Err(format!(
            "kubectl list failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count())
}

/// Writes an absolute-path kubeconfig that points at a closed local port.
pub fn unreachable_kubeconfig(tag: &str) -> Result<(std::path::PathBuf, String)> {
    let path = std::env::temp_dir().join(format!(
        "circus-kubeconfig-{tag}-{}.yaml",
        std::process::id()
    ));
    let contents = "\
apiVersion: v1
kind: Config
clusters:
- name: unreachable
  cluster:
    server: https://127.0.0.1:1
    insecure-skip-tls-verify: true
contexts:
- name: unreachable
  context:
    cluster: unreachable
    user: unreachable
current-context: unreachable
users:
- name: unreachable
  user: {}
";
    std::fs::write(&path, contents)?;
    let text = path.to_string_lossy().to_string();
    Ok((path, text))
}

pub fn is_config_error(error: &SandboxError) -> bool {
    matches!(error, SandboxError::BackendConfig(_))
}

pub fn is_backend_error(error: &SandboxError) -> bool {
    matches!(error, SandboxError::Backend { .. })
}

pub fn image_reference_rejected(error: &SandboxError) -> bool {
    matches!(
        error,
        SandboxError::BackendConfig(BackendConfigError::InvalidImageReference { .. })
    )
}
