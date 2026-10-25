use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use circus_sandbox::{
    BackendConfig, CodeSnapshot, CommandRequest, IoLimits, IsolationConfig, PoolConfig, PoolError,
    PoolStatus, ResourceLimits, Sandbox, SandboxConfig, SandboxPhase, SandboxPool, SnapshotBudget,
    Workspace,
};

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;
const WAIT: Duration = Duration::from_secs(60);
const OWNER_LABEL_KEY: &str = "circus.sandbox.owner";
// Serialize this binary's before/after owner inventories.
static DOCKER_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn backend() -> Result<BackendConfig> {
    assert_eq!(
        std::env::var("CIRCUS_DOCKER_SANDBOX").as_deref(),
        Ok("1"),
        "set CIRCUS_DOCKER_SANDBOX=1 to opt into the real backend tests"
    );
    Ok(BackendConfig::docker(
        std::env::var("CIRCUS_DOCKER_ENDPOINT")?,
        std::env::var("CIRCUS_DOCKER_IMAGE").unwrap_or_else(|_| "python:3.12-slim".into()),
        "runc",
    ))
}

fn config() -> Result<SandboxConfig> {
    Ok(SandboxConfig::new(
        ResourceLimits::new(64 * 1024 * 1024, 100, 32, 8 * 1024 * 1024, 1024)?,
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)?,
        IoLimits::new(64 * 1024, Duration::from_secs(10))?,
    )?)
}

fn docker(backend: &BackendConfig) -> Result<bollard::Docker> {
    let endpoint = match backend {
        BackendConfig::Docker(descriptor) => descriptor.endpoint(),
        BackendConfig::Containerd(_) => panic!("docker fixture received a containerd descriptor"),
        BackendConfig::Kubernetes(_) => panic!("docker fixture received a Kubernetes descriptor"),
    };
    let path = if endpoint.starts_with('/') {
        format!("unix://{endpoint}")
    } else {
        endpoint.to_string()
    };
    Ok(bollard::Docker::connect_with_socket(
        &path,
        30,
        bollard::API_DEFAULT_VERSION,
    )?)
}

async fn absent(docker: &bollard::Docker, id: &str) -> Result {
    match docker.inspect_container(id, None).await {
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(format!("container {id} still exists").into()),
    }
}

async fn owned_ids(docker: &bollard::Docker) -> Result<HashSet<String>> {
    let containers = docker
        .list_containers(Some(bollard::query_parameters::ListContainersOptions {
            all: true,
            filters: Some(HashMap::from([(
                "label".into(),
                vec![OWNER_LABEL_KEY.into()],
            )])),
            ..Default::default()
        }))
        .await?;
    Ok(containers
        .into_iter()
        .filter_map(|container| container.id)
        .collect())
}

async fn eventually_absent(docker: &bollard::Docker, id: &str) -> Result {
    tokio::time::timeout(WAIT, async {
        loop {
            match docker.inspect_container(id, None).await {
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => return Ok(()),
                Err(error) => return Err(Box::<dyn Error>::from(error)),
                Ok(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    })
    .await?
}

async fn id(sandbox: &Sandbox) -> Result<String> {
    let status = sandbox.status().await?;
    assert_eq!(status.phase, SandboxPhase::Running);
    Ok(status
        .instance_id
        .ok_or("running sandbox has no container ID")?)
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

async fn ready(pool: &SandboxPool, count: usize) -> Result {
    tokio::time::timeout(WAIT, async {
        loop {
            let status = pool.status().await?;
            accounted(&status);
            if status.ready == count {
                return Ok::<(), PoolError>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    Ok(())
}

fn baseline(bytes: &[u8]) -> Result<Arc<CodeSnapshot>> {
    let mut source = Workspace::new(4096)?;
    source.write_file("baseline.bin", bytes)?;
    source.create_dir_all("empty/nested")?;
    let snapshot = Arc::new(source.capture_code_snapshot(SnapshotBudget::new(4096, 16)?)?);
    source.cleanup()?;
    Ok(snapshot)
}

fn command(code: &str) -> CommandRequest {
    CommandRequest {
        argv: vec!["python3".into(), "-c".into(), code.into()],
        timeout: Duration::from_secs(10),
        output_limit_bytes: 4096,
    }
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn baseline_single_use_release_and_drop_are_real() -> Result {
    let _serial = DOCKER_TEST.lock().await;
    let backend = backend()?;
    let config = config()?;
    let docker = docker(&backend)?;
    let bytes = b"\0baseline\xff\n";
    let pool = SandboxPool::create(PoolConfig::new(
        backend.clone(),
        config,
        1,
        Some(baseline(bytes)?),
        Duration::from_secs(120),
    )?)
    .await?;
    let initial = pool.status().await?;
    assert_eq!(initial.ready, 1);
    assert_eq!(
        initial.creating + initial.leased + initial.retiring + initial.cleanup_required,
        0
    );
    let first = pool.acquire(WAIT).await?;
    let first_id = id(first.sandbox()).await?;
    assert_eq!(first.sandbox().read_file("baseline.bin").await?, bytes);
    let output = first.sandbox().exec(command(
        "from pathlib import Path; p=Path('/workspace/empty/nested'); assert p.is_dir() and not list(p.iterdir()); Path('/workspace/private').write_bytes(b'tenant-one'); Path('/workspace/baseline.bin').write_bytes(b'dirty'); print('checked')"
    )).await?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"checked\n");
    first.release().await?;
    absent(&docker, &first_id).await?;
    ready(&pool, 1).await?;
    let second = pool.acquire(WAIT).await?;
    let second_id = id(second.sandbox()).await?;
    assert_ne!(first_id, second_id);
    assert_eq!(second.sandbox().read_file("baseline.bin").await?, bytes);
    let output = second.sandbox().exec(command(
        "from pathlib import Path; assert not Path('/workspace/private').exists(); p=Path('/workspace/empty/nested'); assert p.is_dir() and not list(p.iterdir()); print('fresh')"
    )).await?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"fresh\n");
    drop(second);
    eventually_absent(&docker, &second_id).await?;
    ready(&pool, 1).await?;
    let third = pool.acquire(WAIT).await?;
    let third_id = id(third.sandbox()).await?;
    assert_ne!(second_id, third_id);
    pool.shutdown().await?;
    absent(&docker, &third_id).await?;
    drop(third);
    pool.shutdown().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn leased_capacity_times_out_and_cancelled_waiter_does_not_steal_refill() -> Result {
    let _serial = DOCKER_TEST.lock().await;
    let backend = backend()?;
    let docker = docker(&backend)?;
    let pool = SandboxPool::create(PoolConfig::new(
        backend,
        config()?,
        1,
        None,
        Duration::from_secs(120),
    )?)
    .await?;
    let lease = pool.acquire(WAIT).await?;
    let lease_id = id(lease.sandbox()).await?;
    assert!(matches!(
        pool.acquire(Duration::from_millis(100)).await,
        Err(PoolError::AcquireTimeout)
    ));
    let mut cancelled = Box::pin(pool.acquire(WAIT));
    tokio::select! {
        result = &mut cancelled => panic!("occupied pool unexpectedly completed: {:?}", result.err()),
        () = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    drop(cancelled);
    lease.release().await?;
    let next = pool.acquire(WAIT).await?;
    assert_eq!(pool.status().await?.leased, 1);
    let next_id = id(next.sandbox()).await?;
    next.release().await?;
    // Shutdown succeeds only after every owned container is confirmed absent.
    pool.shutdown().await?;
    absent(&docker, &lease_id).await?;
    absent(&docker, &next_id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn concurrent_leases_are_bounded_and_shutdown_revokes_without_pruning() -> Result {
    let _serial = DOCKER_TEST.lock().await;
    let backend = backend()?;
    let config = config()?;
    let docker = docker(&backend)?;
    let survivor = Sandbox::create(backend.clone(), config.clone()).await?;
    survivor.start().await?;
    let survivor_id = id(&survivor).await?;
    let pool = SandboxPool::create(PoolConfig::new(
        backend.clone(),
        config,
        2,
        None,
        Duration::from_secs(120),
    )?)
    .await?;
    let (first, second) = tokio::join!(pool.acquire(WAIT), pool.acquire(WAIT));
    let first = first?;
    let second = second?;
    let ids = [id(first.sandbox()).await?, id(second.sandbox()).await?];
    assert_ne!(ids[0], ids[1]);
    let status = pool.status().await?;
    accounted(&status);
    assert_eq!(status.leased, 2);
    assert_eq!(
        status.ready + status.creating + status.retiring + status.cleanup_required,
        0
    );
    assert!(matches!(
        pool.acquire(Duration::from_millis(100)).await,
        Err(PoolError::AcquireTimeout)
    ));
    let mut pending = Box::pin(pool.acquire(WAIT));
    tokio::select! {
        result = &mut pending => panic!("occupied pool unexpectedly completed: {:?}", result.err()),
        () = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    pool.shutdown().await?;
    assert!(pending.await.is_err());
    assert!(pool.acquire(WAIT).await.is_err());
    for owned in &ids {
        absent(&docker, owned).await?;
    }
    let closed = pool.status().await?;
    assert!(closed.closing);
    assert_eq!(
        closed.ready + closed.creating + closed.leased + closed.retiring + closed.cleanup_required,
        0
    );
    assert!(
        first
            .sandbox()
            .exec(command("print('revoked')"))
            .await
            .is_err()
    );
    assert!(
        second
            .sandbox()
            .exec(command("print('revoked')"))
            .await
            .is_err()
    );
    assert_eq!(id(&survivor).await?, survivor_id);
    assert!(
        docker
            .inspect_container(&survivor_id, None)
            .await?
            .state
            .and_then(|state| state.running)
            .unwrap_or(false)
    );
    let output = survivor.exec(command("print('survivor')")).await?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"survivor\n");
    drop(first);
    drop(second);
    pool.shutdown().await?;
    survivor.destroy().await?;
    absent(&docker, &survivor_id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn preparation_failure_preserves_source_and_any_cleanup_owner() -> Result {
    let _serial = DOCKER_TEST.lock().await;
    let backend = backend()?;
    let config = config()?;
    let docker = docker(&backend)?;
    let before = owned_ids(&docker).await?;
    // A real container can be created, but its 1024-byte file cap cannot restore this baseline.
    let outcome = SandboxPool::create(PoolConfig::new(
        backend.clone(),
        config,
        1,
        Some(baseline(&[7; 2048])?),
        Duration::from_secs(120),
    )?)
    .await;
    let error = match outcome {
        Ok(pool) => {
            pool.shutdown().await?;
            panic!("oversized baseline unexpectedly prepared");
        }
        Err(error) => error,
    };
    assert!(
        error.source().is_some(),
        "preparation failure must retain its structured source"
    );
    match error {
        PoolError::Operation { source } => {
            assert!(matches!(
                source.as_ref(),
                circus_sandbox::PoolFailure::Sandbox { .. }
            ));
            assert!(source.source().is_some());
        }
        PoolError::CleanupRequired { pool, source } => {
            assert!(matches!(
                source.as_ref(),
                circus_sandbox::PoolFailure::Sandbox { .. }
            ));
            assert!(source.source().is_some());
            pool.shutdown().await?;
            let status = pool.status().await?;
            assert!(status.closing);
            assert_eq!(status.absent, status.capacity);
            accounted(&status);
        }
        other => panic!("expected structured preparation error, got {other:?}"),
    }
    let after = owned_ids(&docker).await?;
    assert!(
        after.is_subset(&before),
        "failed preparation leaked owned containers: {:?}",
        after.difference(&before).collect::<Vec<_>>()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn lease_ttl_revokes_running_child_and_refills_without_touching_survivor() -> Result {
    let _serial = DOCKER_TEST.lock().await;
    let backend = backend()?;
    let config = config()?;
    let docker = docker(&backend)?;
    let survivor = Sandbox::create(backend.clone(), config.clone()).await?;
    survivor.start().await?;
    let survivor_id = id(&survivor).await?;
    let pool = SandboxPool::create(PoolConfig::new(
        backend.clone(),
        config,
        1,
        None,
        Duration::from_secs(5),
    )?)
    .await?;
    let expired = pool.acquire(WAIT).await?;
    let expired_id = id(expired.sandbox()).await?;
    expired
        .sandbox()
        .write_file("private", b"expired tenant")
        .await?;
    let request = CommandRequest {
        argv: vec!["python3".into(), "-c".into(),
            "import subprocess,sys; subprocess.run([sys.executable,'-c','import time; time.sleep(60)'],check=True)".into()],
        timeout: Duration::from_secs(90),
        output_limit_bytes: 4096,
    };
    let execution =
        tokio::time::timeout(Duration::from_secs(30), expired.sandbox().exec(request)).await?;
    assert!(
        matches!(
            &execution,
            Err(circus_sandbox::SandboxError::Cancelled { .. })
                | Err(circus_sandbox::SandboxError::WorkerGone { .. })
        ),
        "TTL must interrupt the long-running command, got {execution:?}"
    );
    // Expiry requests revocation; only an actual inspect 404 confirms destruction.
    eventually_absent(&docker, &expired_id).await?;
    ready(&pool, 1).await?;
    let fresh = pool.acquire(WAIT).await?;
    let fresh_id = id(fresh.sandbox()).await?;
    assert_ne!(expired_id, fresh_id);
    let output = fresh.sandbox().exec(command(
        "from pathlib import Path; assert not Path('/workspace/private').exists(); print('fresh after expiry')"
    )).await?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"fresh after expiry\n");
    assert_eq!(id(&survivor).await?, survivor_id);
    let output = survivor.exec(command("print('unaffected')")).await?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"unaffected\n");
    fresh.release().await?;
    absent(&docker, &fresh_id).await?;
    drop(expired);
    pool.shutdown().await?;
    survivor.destroy().await?;
    absent(&docker, &survivor_id).await?;
    Ok(())
}
