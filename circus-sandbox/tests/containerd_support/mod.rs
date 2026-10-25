//! Shared fixtures for the opt-in real containerd suites.
//!
//! Every suite is ignored unless `CIRCUS_CONTAINERD=1`. Absence is asserted
//! with the real `ctr` CLI in the same namespace, never with a mock.

#![allow(dead_code)]

use std::process::{Command, Output};
use std::time::Duration;

use circus_sandbox::{
    BackendConfig, CommandRequest, IoLimits, IsolationConfig, ResourceLimits, Sandbox,
    SandboxConfig, SnapshotBudget,
};

pub type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

pub fn enabled() {
    assert_eq!(
        std::env::var("CIRCUS_CONTAINERD").as_deref(),
        Ok("1"),
        "set CIRCUS_CONTAINERD=1 to opt into the real containerd suites"
    );
}

pub fn socket() -> String {
    std::env::var("CIRCUS_CONTAINERD_SOCKET")
        .unwrap_or_else(|_| "/run/containerd/containerd.sock".to_string())
}

pub fn namespace() -> String {
    std::env::var("CIRCUS_CONTAINERD_NAMESPACE").unwrap_or_else(|_| "default".to_string())
}

pub fn image() -> String {
    std::env::var("CIRCUS_CONTAINERD_IMAGE").unwrap_or_else(|_| "python:3.12-slim".to_string())
}

pub fn backend() -> BackendConfig {
    BackendConfig::containerd(socket(), namespace(), image(), "runc", "overlayfs")
}

pub fn backend_in(namespace: &str, image: &str) -> BackendConfig {
    BackendConfig::containerd(socket(), namespace, image, "runc", "overlayfs")
}

pub fn config() -> Result<SandboxConfig> {
    Ok(SandboxConfig::new(
        ResourceLimits::new(128 * 1024 * 1024, 1000, 64, 16 * 1024 * 1024, 1024 * 1024)?,
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)?,
        // Shared CI runners are slower than a local WSL daemon; keep a bounded
        // but non-flaky lifecycle deadline for container/task startup.
        IoLimits::with_deadlines(
            4 * 1024 * 1024,
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(60),
        )?,
    )?)
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

pub fn budget() -> Result<SnapshotBudget> {
    Ok(SnapshotBudget::new(1024 * 1024, 64)?)
}

pub fn ctr(namespace: &str, args: &[&str]) -> Result<Output> {
    Ok(Command::new("ctr")
        .args(["-n", namespace])
        .args(args)
        .output()?)
}

pub fn create_namespace(namespace: &str) -> Result {
    let output = Command::new("ctr")
        .args(["namespaces", "create", namespace])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        if !stderr.contains("already exists") {
            return Err(format!("ctr namespace create failed: {stderr}").into());
        }
    }
    Ok(())
}

/// Confirms with `ctr` that neither the container nor its snapshot key exists.
pub fn assert_absent(namespace: &str, id: &str) -> Result {
    let containers = ctr(namespace, &["containers", "list", "-q"])?;
    if !containers.status.success() {
        return Err(format!(
            "ctr containers list failed: {}",
            String::from_utf8_lossy(&containers.stderr)
        )
        .into());
    }
    let listed = String::from_utf8_lossy(&containers.stdout);
    if listed.lines().any(|line| line.trim() == id) {
        return Err(format!("container {id} is still listed by ctr").into());
    }
    let snapshots = ctr(
        namespace,
        &["snapshots", "--snapshotter", "overlayfs", "list"],
    )?;
    if !snapshots.status.success() {
        return Err(format!(
            "ctr snapshots list failed: {}",
            String::from_utf8_lossy(&snapshots.stderr)
        )
        .into());
    }
    let listed = String::from_utf8_lossy(&snapshots.stdout);
    for line in listed.lines().skip(1) {
        if line.split_whitespace().next() == Some(id) {
            return Err(format!("snapshot {id} is still listed by ctr").into());
        }
    }
    Ok(())
}

pub async fn destroy_and_assert(sandbox: &Sandbox, id: &str) -> Result {
    sandbox.destroy().await?;
    assert_absent(&namespace(), id)
}

/// Asserts a fresh namespace has no containers or snapshots at all.
pub fn assert_namespace_empty(namespace: &str) -> Result {
    let containers = ctr(namespace, &["containers", "list", "-q"])?;
    if !containers.status.success() {
        return Err("ctr containers list failed".into());
    }
    if !String::from_utf8_lossy(&containers.stdout)
        .trim()
        .is_empty()
    {
        return Err("containers remain in the fresh namespace".into());
    }
    let snapshots = ctr(
        namespace,
        &["snapshots", "--snapshotter", "overlayfs", "list"],
    )?;
    if !snapshots.status.success() {
        return Err("ctr snapshots list failed".into());
    }
    let listed = String::from_utf8_lossy(&snapshots.stdout);
    if listed.lines().skip(1).any(|line| !line.trim().is_empty()) {
        return Err("snapshots remain in the fresh namespace".into());
    }
    Ok(())
}
