use std::time::Duration;

use circus_sandbox::runtime::ResourceLimits;
use circus_sandbox::{
    BackendConfig, IoLimits, IsolationConfig, PoolConfig, PoolError, SandboxConfig, SandboxMetrics,
};

fn sandbox_config() -> SandboxConfig {
    SandboxConfig::new(
        ResourceLimits::new(64 * 1024, 100, 3, 32 * 1024, 1024).unwrap(),
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 1000, 1000).unwrap(),
        IoLimits::new(4096, Duration::from_secs(2)).unwrap(),
    )
    .unwrap()
}

fn backend() -> BackendConfig {
    BackendConfig::docker("unix:///var/run/docker.sock", "alpine:3.20", "runc")
}

#[test]
fn rejects_zero_capacity() {
    assert!(matches!(
        PoolConfig::new(
            backend(),
            sandbox_config(),
            0,
            None,
            Duration::from_secs(30)
        ),
        Err(PoolError::InvalidCapacity)
    ));
}

#[test]
fn rejects_zero_lease_ttl() {
    assert!(matches!(
        PoolConfig::new(backend(), sandbox_config(), 1, None, Duration::ZERO),
        Err(PoolError::InvalidLeaseTtl)
    ));
}

#[test]
fn accepts_valid_config_and_attaches_metrics() {
    let config = PoolConfig::new(
        backend(),
        sandbox_config(),
        2,
        None,
        Duration::from_secs(30),
    )
    .expect("valid pool config");
    let _ = config.with_metrics(SandboxMetrics::new());
}
