//! Real containerd sandbox-pool suite: warm capacity, single-use leases,
//! release/TTL/shutdown destruction, and typed preparation failure.
//!
//! Every test is ignored unless `CIRCUS_CONTAINERD=1`. Absence is asserted with
//! the real `ctr` CLI in the same namespace, including the snapshot key; the
//! default namespace holds the test image, so positive tests identify their own
//! containers by ID instead of assuming an empty namespace.

mod containerd_support;

use std::collections::HashSet;
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use circus_sandbox::{
    BackendConfig, CommandRequest, PoolConfig, PoolError, PoolFailure, PoolLease, PoolStatus,
    SandboxError, SandboxPhase, SandboxPool,
};
use containerd_support as support;

const WAIT: Duration = Duration::from_secs(90);
static POOL_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static NEXT_NAMESPACE: AtomicUsize = AtomicUsize::new(0);

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
        .ok_or("running pool sandbox has no containerd id")?)
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

async fn eventually_absent(id: &str) -> support::Result {
    let namespace = support::namespace();
    tokio::time::timeout(WAIT, async {
        loop {
            if support::assert_absent(&namespace, id).is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| format!("container {id} and its snapshot did not disappear"))?;
    support::assert_absent(&namespace, id)
}

fn containers(namespace: &str) -> support::Result<HashSet<String>> {
    let output = support::ctr(namespace, &["containers", "list", "-q"])?;
    if !output.status.success() {
        return Err(format!(
            "ctr containers list failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn assert_no_leftovers(before: &HashSet<String>, namespace: &str) -> support::Result {
    let after = containers(namespace)?;
    assert!(
        after.is_subset(before),
        "pool left containerd containers behind: {:?}",
        after.difference(before).collect::<Vec<_>>()
    );
    Ok(())
}

fn python(code: &str) -> CommandRequest {
    CommandRequest {
        argv: vec!["python3".into(), "-c".into(), code.into()],
        timeout: Duration::from_secs(90),
        output_limit_bytes: 4096,
    }
}

fn assert_missing_image_failure(source: &PoolFailure) {
    match source {
        PoolFailure::Sandbox { operation, source } => {
            assert_eq!(*operation, "create");
            match source {
                SandboxError::BackendImageUnavailable { image } => {
                    assert_eq!(image, "docker.io/library/circus-does-not-exist:1");
                }
                other => panic!("expected a typed missing-image error, got {other:?}"),
            }
        }
        other => panic!("expected a sandbox preparation failure, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires an explicit real containerd daemon"]
async fn warm_capacity_is_prestarted_and_status_is_accounted() -> support::Result {
    support::enabled();
    let _serial = POOL_TEST.lock().await;
    let namespace = support::namespace();
    let before = containers(&namespace)?;
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

    let (first, second) = tokio::join!(pool.acquire(WAIT), pool.acquire(WAIT));
    let first = first?;
    let second = second?;
    let first_id = id_of(&first).await?;
    let second_id = id_of(&second).await?;
    assert_ne!(first_id, second_id);
    let listed = containers(&namespace)?;
    assert!(
        listed.contains(&first_id),
        "warm container {first_id} is not in ctr"
    );
    assert!(
        listed.contains(&second_id),
        "warm container {second_id} is not in ctr"
    );
    let leased = pool.status().await?;
    accounted(&leased);
    assert_eq!(leased.leased, 2);
    assert_eq!(
        leased.ready + leased.creating + leased.retiring + leased.cleanup_required,
        0
    );

    first.release().await?;
    eventually_absent(&first_id).await?;
    second.release().await?;
    eventually_absent(&second_id).await?;
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
    assert_no_leftovers(&before, &namespace)?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real containerd daemon"]
async fn release_destroys_container_and_snapshot_and_next_lease_is_fresh() -> support::Result {
    support::enabled();
    let _serial = POOL_TEST.lock().await;
    let namespace = support::namespace();
    let before = containers(&namespace)?;
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
        .exec(python(
            "from pathlib import Path\nPath('/workspace/private').write_bytes(b'tenant-one')\nprint('wrote')\n",
        ))
        .await?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"wrote\n");
    first.release().await?;
    // `assert_absent` checks both the container and its snapshot key.
    support::assert_absent(&namespace, &first_id)?;
    ready(&pool, 1).await?;

    let second = pool.acquire(WAIT).await?;
    let second_id = id_of(&second).await?;
    assert_ne!(first_id, second_id);
    let output = second
        .sandbox()
        .exec(python(
            "from pathlib import Path\nassert not Path('/workspace/private').exists()\nprint('fresh')\n",
        ))
        .await?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"fresh\n");
    second.release().await?;
    support::assert_absent(&namespace, &second_id)?;
    pool.shutdown().await?;
    assert_no_leftovers(&before, &namespace)?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real containerd daemon"]
async fn capacity_one_times_out_while_leased_then_succeeds_after_release() -> support::Result {
    support::enabled();
    let _serial = POOL_TEST.lock().await;
    let namespace = support::namespace();
    let before = containers(&namespace)?;
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
    eventually_absent(&first_id).await?;

    let second = pool.acquire(WAIT).await?;
    let second_id = id_of(&second).await?;
    assert_ne!(first_id, second_id);
    assert_eq!(pool.status().await?.leased, 1);
    second.release().await?;
    support::assert_absent(&namespace, &second_id)?;
    pool.shutdown().await?;
    assert_no_leftovers(&before, &namespace)?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real containerd daemon"]
async fn lease_ttl_revokes_in_flight_exec_and_refills_with_a_fresh_container() -> support::Result {
    support::enabled();
    let _serial = POOL_TEST.lock().await;
    let namespace = support::namespace();
    let before = containers(&namespace)?;
    let pool =
        SandboxPool::create(pool_config(support::backend(), 1, Duration::from_secs(3))?).await?;

    let expired = pool.acquire(WAIT).await?;
    let expired_id = id_of(&expired).await?;
    expired
        .sandbox()
        .write_file("private", b"expired tenant")
        .await?;
    let request = python(
        "import subprocess,sys\nsubprocess.run([sys.executable,'-c','import time; time.sleep(60)'],check=True)\n",
    );
    let execution =
        tokio::time::timeout(Duration::from_secs(30), expired.sandbox().exec(request)).await?;
    assert!(
        matches!(
            &execution,
            Err(SandboxError::Cancelled { .. }) | Err(SandboxError::WorkerGone { .. })
        ),
        "TTL must interrupt the long-running command, got {execution:?}"
    );
    // Expiry requests revocation; only an actual ctr absence confirms destruction.
    eventually_absent(&expired_id).await?;
    ready(&pool, 1).await?;

    let fresh = pool.acquire(WAIT).await?;
    let fresh_id = id_of(&fresh).await?;
    assert_ne!(expired_id, fresh_id);
    let output = fresh
        .sandbox()
        .exec(python(
            "from pathlib import Path\nassert not Path('/workspace/private').exists()\nprint('fresh after expiry')\n",
        ))
        .await?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"fresh after expiry\n");
    fresh.release().await?;
    support::assert_absent(&namespace, &fresh_id)?;
    drop(expired);
    pool.shutdown().await?;
    assert_no_leftovers(&before, &namespace)?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real containerd daemon"]
async fn shutdown_revokes_outstanding_lease_and_is_idempotent() -> support::Result {
    support::enabled();
    let _serial = POOL_TEST.lock().await;
    let namespace = support::namespace();
    let before = containers(&namespace)?;
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
    support::assert_absent(&namespace, &lease_id)?;
    let closed = pool.status().await?;
    accounted(&closed);
    assert!(closed.closing);
    assert_eq!(closed.absent, 2);
    assert_eq!(
        closed.ready + closed.creating + closed.leased + closed.retiring + closed.cleanup_required,
        0
    );
    assert!(
        lease
            .sandbox()
            .exec(python("print('revoked')"))
            .await
            .is_err()
    );
    assert!(matches!(pool.acquire(WAIT).await, Err(PoolError::Closing)));
    pool.shutdown().await?;
    drop(lease);
    pool.shutdown().await?;
    assert_no_leftovers(&before, &namespace)?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real containerd daemon"]
async fn failing_preparation_leaves_no_container_and_a_typed_error() -> support::Result {
    support::enabled();
    let _serial = POOL_TEST.lock().await;
    let namespace = format!(
        "circus-pool-missing-{}-{}",
        std::process::id(),
        NEXT_NAMESPACE.fetch_add(1, Ordering::Relaxed)
    );
    support::create_namespace(&namespace)?;
    let backend = support::backend_in(&namespace, "docker.io/library/circus-does-not-exist:1");
    let outcome = SandboxPool::create(pool_config(backend, 1, Duration::from_secs(120))?).await;
    let error = match outcome {
        Ok(pool) => {
            pool.shutdown().await?;
            panic!("a missing image unexpectedly prepared a pool");
        }
        Err(error) => error,
    };
    assert!(
        error.source().is_some(),
        "preparation failure must retain its structured source"
    );
    match error {
        PoolError::Operation { source } => assert_missing_image_failure(&source),
        PoolError::CleanupRequired { pool, source } => {
            assert_missing_image_failure(&source);
            pool.shutdown().await?;
            let status = pool.status().await?;
            accounted(&status);
            assert!(status.closing);
            assert_eq!(status.absent, status.capacity);
        }
        other => panic!("expected a structured preparation error, got {other:?}"),
    }
    // Prepare fails before a container or snapshot is allocated.
    support::assert_namespace_empty(&namespace)?;
    let removed = support::ctr(&namespace, &["namespaces", "remove", namespace.as_str()])?;
    assert!(
        removed.status.success(),
        "namespace cleanup failed: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    Ok(())
}
