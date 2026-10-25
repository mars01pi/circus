//! Real Kubernetes sandbox-pool suite: warm capacity, single-use leases,
//! release/TTL/shutdown destruction, and typed preparation failure.
//!
//! Every test is ignored unless `CIRCUS_KUBERNETES=1`. Absence is asserted with
//! the real `kubectl` CLI in the same namespace, never with a mock. Run
//! serially (`--test-threads=1`) because these tests share the namespace.

mod kubernetes_support;

use std::time::Duration;

use circus_sandbox::{
    BackendConfig, CommandRequest, PoolConfig, PoolError, PoolFailure, PoolLease, PoolStatus,
    SandboxError, SandboxPhase, SandboxPool,
};
use kubernetes_support as support;

const WAIT: Duration = Duration::from_secs(120);
static POOL_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn pool_config(
    backend: BackendConfig,
    capacity: usize,
    lease_ttl: Duration,
) -> support::Result<PoolConfig> {
    Ok(PoolConfig::new(
        backend,
        support::config()?,
        capacity,
        None,
        lease_ttl,
    )?)
}

async fn id_of(lease: &PoolLease) -> support::Result<String> {
    let status = lease.sandbox().status().await?;
    assert_eq!(status.phase, SandboxPhase::Running);
    Ok(status
        .instance_id
        .ok_or("running pool sandbox has no Pod name")?)
}

fn accounted(status: &PoolStatus) {
    assert_eq!(
        status.ready
            + status.creating
            + status.leased
            + status.retiring
            + status.cleanup_required
            + status.absent,
        status.capacity
    );
}

async fn ready(pool: &SandboxPool, count: usize) -> support::Result {
    tokio::time::timeout(WAIT, async {
        loop {
            let status = pool.status().await?;
            accounted(&status);
            if status.ready == count {
                return Ok::<(), PoolError>(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| "pool did not reach the required ready count")??;
    Ok(())
}

fn long_python(code: &str) -> CommandRequest {
    CommandRequest {
        argv: vec!["python3".into(), "-c".into(), code.into()],
        timeout: Duration::from_secs(90),
        output_limit_bytes: 4096,
    }
}

fn assert_backend_preparation_failure(source: &PoolFailure) {
    match source {
        PoolFailure::Sandbox { operation, source } => {
            assert_eq!(*operation, "create");
            assert!(
                matches!(source, SandboxError::Backend { .. }),
                "expected a typed backend preparation error, got {source:?}"
            );
        }
        other => panic!("expected a sandbox preparation failure, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_pool_warm_capacity_and_release_confirm_pod_absence() -> support::Result {
    support::enabled();
    let _serial = POOL_TEST.lock().await;
    let before = support::managed_pod_count()?;
    let pool = SandboxPool::create(pool_config(
        support::backend(),
        2,
        Duration::from_secs(120),
    )?)
    .await?;

    let warm = pool.status().await?;
    accounted(&warm);
    assert_eq!(warm.capacity, 2);
    assert_eq!(warm.ready, 2);
    assert_eq!(
        warm.creating + warm.leased + warm.retiring + warm.cleanup_required + warm.absent,
        0
    );
    assert!(!warm.closing);
    assert_eq!(support::managed_pod_count()?, before + 2);

    let (first, second) = tokio::join!(pool.acquire(WAIT), pool.acquire(WAIT));
    let first = first?;
    let second = second?;
    let first_id = id_of(&first).await?;
    let second_id = id_of(&second).await?;
    assert_ne!(first_id, second_id);
    assert!(support::pod_exists(&first_id)?);
    assert!(support::pod_exists(&second_id)?);
    let leased = pool.status().await?;
    accounted(&leased);
    assert_eq!(leased.leased, 2);
    assert_eq!(
        leased.ready + leased.creating + leased.retiring + leased.cleanup_required,
        0
    );

    first.release().await?;
    support::eventually_absent(&first_id).await?;
    second.release().await?;
    support::eventually_absent(&second_id).await?;
    ready(&pool, 2).await?;

    pool.shutdown().await?;
    let closed = pool.status().await?;
    accounted(&closed);
    assert!(closed.closing);
    assert_eq!(closed.absent, 2);
    assert_eq!(
        closed.ready + closed.creating + closed.leased + closed.retiring + closed.cleanup_required,
        0
    );
    assert_eq!(support::managed_pod_count()?, before);
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_pool_markers_do_not_cross_leases_and_release_destroys_the_pod()
-> support::Result {
    support::enabled();
    let _serial = POOL_TEST.lock().await;
    let before = support::managed_pod_count()?;
    let pool = SandboxPool::create(pool_config(
        support::backend(),
        1,
        Duration::from_secs(120),
    )?)
    .await?;

    let first = pool.acquire(WAIT).await?;
    let first_id = id_of(&first).await?;
    let output = first
        .sandbox()
        .exec(support::python(&[
            "-c",
            "from pathlib import Path\nPath('/workspace/private').write_bytes(b'tenant-one')\nprint('wrote')\n",
        ]))
        .await?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"wrote\n");
    first.release().await?;
    support::eventually_absent(&first_id).await?;
    ready(&pool, 1).await?;

    let second = pool.acquire(WAIT).await?;
    let second_id = id_of(&second).await?;
    assert_ne!(first_id, second_id);
    let output = second
        .sandbox()
        .exec(support::python(&[
            "-c",
            "from pathlib import Path\nassert not Path('/workspace/private').exists()\nprint('fresh')\n",
        ]))
        .await?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"fresh\n");
    second.release().await?;
    support::eventually_absent(&second_id).await?;

    pool.shutdown().await?;
    assert_eq!(support::managed_pod_count()?, before);
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_pool_capacity_one_times_out_then_succeeds_after_release() -> support::Result {
    support::enabled();
    let _serial = POOL_TEST.lock().await;
    let before = support::managed_pod_count()?;
    let pool = SandboxPool::create(pool_config(
        support::backend(),
        1,
        Duration::from_secs(120),
    )?)
    .await?;

    let first = pool.acquire(WAIT).await?;
    let first_id = id_of(&first).await?;
    assert!(matches!(
        pool.acquire(Duration::from_millis(200)).await,
        Err(PoolError::AcquireTimeout)
    ));
    first.release().await?;
    support::eventually_absent(&first_id).await?;

    let second = pool.acquire(WAIT).await?;
    let second_id = id_of(&second).await?;
    assert_ne!(first_id, second_id);
    assert_eq!(pool.status().await?.leased, 1);
    second.release().await?;
    support::eventually_absent(&second_id).await?;

    pool.shutdown().await?;
    assert_eq!(support::managed_pod_count()?, before);
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_pool_lease_ttl_revokes_in_flight_exec_and_refills() -> support::Result {
    support::enabled();
    let _serial = POOL_TEST.lock().await;
    let before = support::managed_pod_count()?;
    let pool =
        SandboxPool::create(pool_config(support::backend(), 1, Duration::from_secs(10))?).await?;

    let expired = pool.acquire(WAIT).await?;
    let expired_id = id_of(&expired).await?;
    expired
        .sandbox()
        .write_file("private", b"expired tenant")
        .await?;
    let execution = tokio::time::timeout(
        Duration::from_secs(60),
        expired
            .sandbox()
            .exec(long_python("import time; time.sleep(60)")),
    )
    .await?;
    assert!(
        matches!(
            &execution,
            Err(SandboxError::Cancelled { .. }) | Err(SandboxError::WorkerGone { .. })
        ),
        "TTL must interrupt the long-running command, got {execution:?}"
    );
    // Expiry requests revocation; only kubectl absence confirms destruction.
    support::eventually_absent(&expired_id).await?;
    ready(&pool, 1).await?;

    let fresh = pool.acquire(WAIT).await?;
    let fresh_id = id_of(&fresh).await?;
    assert_ne!(expired_id, fresh_id);
    let output = fresh
        .sandbox()
        .exec(support::python(&[
            "-c",
            "from pathlib import Path\nassert not Path('/workspace/private').exists()\nprint('fresh after expiry')\n",
        ]))
        .await?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"fresh after expiry\n");
    fresh.release().await?;
    support::eventually_absent(&fresh_id).await?;
    drop(expired);

    pool.shutdown().await?;
    assert_eq!(support::managed_pod_count()?, before);
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_pool_shutdown_revokes_leases_and_is_idempotent() -> support::Result {
    support::enabled();
    let _serial = POOL_TEST.lock().await;
    let before = support::managed_pod_count()?;
    let pool = SandboxPool::create(pool_config(
        support::backend(),
        2,
        Duration::from_secs(120),
    )?)
    .await?;

    let lease = pool.acquire(WAIT).await?;
    let lease_id = id_of(&lease).await?;
    let held = pool.status().await?;
    accounted(&held);
    assert_eq!(held.leased, 1);
    assert_eq!(held.ready, 1);

    // Shutdown revokes the outstanding lease as well as the idle slot.
    pool.shutdown().await?;
    support::assert_absent(&lease_id)?;
    let closed = pool.status().await?;
    accounted(&closed);
    assert!(closed.closing);
    assert_eq!(closed.absent, 2);
    assert_eq!(
        closed.ready + closed.creating + closed.leased + closed.retiring + closed.cleanup_required,
        0
    );
    assert_eq!(support::managed_pod_count()?, before);
    assert!(
        lease
            .sandbox()
            .exec(support::python(&["-c", "print('revoked')"]))
            .await
            .is_err()
    );
    assert!(matches!(pool.acquire(WAIT).await, Err(PoolError::Closing)));
    pool.shutdown().await?;
    drop(lease);
    pool.shutdown().await?;
    assert_eq!(support::managed_pod_count()?, before);
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_pool_failing_preparation_leaves_no_pod_and_a_typed_error() -> support::Result {
    support::enabled();
    let _serial = POOL_TEST.lock().await;
    let before = support::managed_pod_count()?;
    let (path, kubeconfig) = support::unreachable_kubeconfig("pool")?;
    let backend = BackendConfig::kubernetes(&kubeconfig, support::namespace(), support::image());
    let outcome = SandboxPool::create(pool_config(backend, 1, Duration::from_secs(120))?).await;
    std::fs::remove_file(path).ok();
    let error = match outcome {
        Ok(pool) => {
            pool.shutdown().await?;
            panic!("an unreachable API unexpectedly prepared a pool");
        }
        Err(error) => error,
    };
    assert!(
        std::error::Error::source(&error).is_some(),
        "preparation failure must retain its structured source"
    );
    match error {
        PoolError::Operation { source } => assert_backend_preparation_failure(&source),
        PoolError::CleanupRequired { pool, source } => {
            assert_backend_preparation_failure(&source);
            pool.shutdown().await?;
            let status = pool.status().await?;
            accounted(&status);
            assert!(status.closing);
            assert_eq!(status.absent, status.capacity);
        }
        other => panic!("expected a structured preparation error, got {other:?}"),
    }
    // Preparation fails before any Pod is allocated.
    assert_eq!(support::managed_pod_count()?, before);
    Ok(())
}
