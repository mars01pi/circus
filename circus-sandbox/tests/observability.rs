//! Public caller-owned metrics API. No Docker Engine is required.

use std::time::Duration;

use circus_sandbox::{
    BackendConfig, IoLimits, IsolationConfig, MetricsSnapshot, OperationKind, OperationOutcome,
    ResourceLimits, Sandbox, SandboxConfig, SandboxError, SandboxMetrics, TerminationKind,
};

fn invalid_backend() -> BackendConfig {
    BackendConfig::docker("tcp://127.0.0.1:2375", "python:3.12-slim", "runc")
}

fn config() -> SandboxConfig {
    let limits =
        ResourceLimits::new(8 * 1024 * 1024, 100, 8, 1024 * 1024, 4096).expect("valid limits");
    let isolation =
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)
            .expect("valid isolation");
    SandboxConfig::new(
        limits,
        isolation,
        IoLimits::new(64 * 1024, Duration::from_secs(1)).expect("valid io limits"),
    )
    .expect("valid sandbox config")
}

#[test]
fn records_every_fixed_counter() {
    let metrics = SandboxMetrics::new();
    assert!(metrics.is_enabled());
    metrics.record_create_ok();
    metrics.record_create_failed();
    metrics.record_create_cleanup_pending();
    metrics.record_operation(OperationKind::Start, OperationOutcome::Ok);
    metrics.record_operation(OperationKind::Exec, OperationOutcome::Cancelled);
    metrics.record_cleanup_attempt();
    metrics.record_cleanup_failure();
    metrics.record_pool_ready();
    metrics.record_pool_leased();
    metrics.record_pool_retired();
    metrics.record_pool_refilled();
    metrics.record_snapshot_capture(true);
    metrics.record_snapshot_restore(false);
    metrics.record_process_started();
    metrics.record_process_ended(TerminationKind::TimedOut);
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.creates_ok, 1);
    assert_eq!(snapshot.creates_failed, 1);
    assert_eq!(snapshot.create_cleanup_pending, 1);
    assert_eq!(snapshot.create_cleanup_completed, 0);
    assert_eq!(snapshot.operation(OperationKind::Start).ok, 1);
    assert_eq!(snapshot.operation(OperationKind::Exec).cancelled, 1);
    assert_eq!(snapshot.operation(OperationKind::Start).error, 0);
    assert_eq!(snapshot.cleanup_attempts, 1);
    assert_eq!(snapshot.cleanup_failures, 1);
    assert_eq!(snapshot.pool_ready, 1);
    assert_eq!(snapshot.pool_leased, 1);
    assert_eq!(snapshot.pool_retired, 1);
    assert_eq!(snapshot.pool_refilled, 1);
    assert_eq!(snapshot.snapshot_captures_ok, 1);
    assert_eq!(snapshot.snapshot_restores_failed, 1);
    assert_eq!(snapshot.process_sessions_started, 1);
    assert_eq!(snapshot.process_terminated(TerminationKind::TimedOut), 1);
}

#[test]
fn cleanup_completion_balances_the_pending_gauge() {
    let metrics = SandboxMetrics::new();
    metrics.record_create_cleanup_pending();
    metrics.record_create_cleanup_completed();
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.create_cleanup_pending, 0);
    assert_eq!(snapshot.create_cleanup_completed, 1);
}

#[test]
fn clones_share_one_counter_set() {
    let metrics = SandboxMetrics::new();
    let clone = metrics.clone();
    metrics.record_snapshot_capture(true);
    clone.record_snapshot_capture(true);
    assert_eq!(metrics.snapshot().snapshot_captures_ok, 2);
    assert_eq!(clone.snapshot().snapshot_captures_ok, 2);
}

#[test]
fn default_and_disabled_handles_record_nothing() {
    for metrics in [SandboxMetrics::default(), SandboxMetrics::disabled()] {
        assert!(!metrics.is_enabled());
        metrics.record_create_ok();
        metrics.record_create_failed();
        metrics.record_operation(OperationKind::Exec, OperationOutcome::Error);
        metrics.record_cleanup_attempt();
        metrics.record_snapshot_restore(true);
        metrics.record_process_started();
        metrics.record_process_ended(TerminationKind::Exited);
        assert_eq!(metrics.snapshot(), MetricsSnapshot::default());
    }
}

#[tokio::test]
async fn failed_create_updates_shared_metrics() {
    let metrics = SandboxMetrics::new();
    let result = Sandbox::create_with_metrics(invalid_backend(), config(), metrics.clone()).await;
    assert!(matches!(result, Err(SandboxError::BackendConfig(_))));
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.creates_ok, 0);
    assert_eq!(snapshot.creates_failed, 1);
    assert_eq!(snapshot.create_cleanup_pending, 0);
}
