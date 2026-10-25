// Opt-in real Docker Engine tests for aggregate output, delete ownership,
// operation deadlines, and post-create policy verification.
//
// Every test requires CIRCUS_DOCKER_SANDBOX=1 and CIRCUS_DOCKER_ENDPOINT and is
// marked #[ignore]. They were not run in the environment that produced this
// change because Docker was unavailable; they contain no simulated backend.
//
// The tests serialize themselves because a few observe the engine's global
// container set used by this file.

use std::process::Command;
use std::time::Duration;

use circus_sandbox::{
    BackendConfig, CommandRequest, IoLimits, IsolationConfig, OutputPhase, Ownership,
    ResourceLimits, Sandbox, SandboxConfig, SandboxError, SandboxOperation, SandboxPhase,
};

const PY: &str = "python3";
const TEST_PIDS: u32 = 32;
const MEMORY: u64 = 64 * 1024 * 1024;
const WORKSPACE: u64 = 8 * 1024 * 1024;
const OWNER_LABEL: &str = "circus.sandbox.owner";

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn enabled() {
    assert_eq!(
        std::env::var("CIRCUS_DOCKER_SANDBOX").as_deref(),
        Ok("1"),
        "set CIRCUS_DOCKER_SANDBOX=1 to opt into the real backend test"
    );
}

fn backend() -> Result<BackendConfig, Box<dyn std::error::Error>> {
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

fn config() -> Result<SandboxConfig, Box<dyn std::error::Error>> {
    config_with_lifecycle(Duration::from_secs(10))
}

fn config_with_lifecycle(
    lifecycle_timeout: Duration,
) -> Result<SandboxConfig, Box<dyn std::error::Error>> {
    let limits = ResourceLimits::new(MEMORY, 100, TEST_PIDS, WORKSPACE, 1024)?;
    Ok(SandboxConfig::new(
        limits,
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)?,
        IoLimits::with_deadlines(
            64 * 1024,
            Duration::from_secs(2),
            Duration::from_secs(5),
            lifecycle_timeout,
        )?,
    )?)
}

fn docker(host: &str, args: &[&str]) -> Result<std::process::Output, Box<dyn std::error::Error>> {
    Ok(Command::new("docker")
        .arg("--host")
        .arg(host)
        .args(args)
        .output()?)
}

fn inspect_exists(host: &str, id: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let output = docker(host, &["inspect", id])?;
    Ok(output.status.success())
}

fn require_inspect(host: &str, id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let output = docker(host, &["inspect", id])?;
    assert!(
        output.status.success(),
        "inspect of {id} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn owned_container_ids(host: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let output = docker(
        host,
        &["ps", "-aq", "--filter", &format!("label={OWNER_LABEL}")],
    )?;
    assert!(
        output.status.success(),
        "docker ps failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn owned_container_count(host: &str) -> Result<usize, Box<dyn std::error::Error>> {
    Ok(owned_container_ids(host)?.len())
}

struct Fixture {
    sandbox: Option<Sandbox>,
    host: String,
    id: Option<String>,
}

impl Fixture {
    async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Self::with_config(backend()?, config()?).await
    }

    async fn with_config(
        backend: BackendConfig,
        config: SandboxConfig,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let host = host(&backend);
        // Identify the new container by diffing the engine's owned set. This
        // avoids a status call, which a deliberately tiny status deadline
        // would interrupt.
        let before = owned_container_ids(&host)?;
        let sandbox = Sandbox::create(backend, config).await?;
        let after = owned_container_ids(&host)?;
        let id = after
            .into_iter()
            .find(|candidate| !before.contains(candidate));
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

// The fixture owns exactly one container. Teardown is explicit and has no Drop
// fallback: `Sandbox::destroy` reports success only after confirmed absence,
// then the engine must also report the container gone. Unconfirmed removal
// fails the test instead of being ignored.
async fn teardown(f: &mut Fixture) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(sandbox) = f.sandbox.take() {
        sandbox.destroy().await?;
    }
    let Some(id) = f.id.as_deref() else {
        return Err("fixture has no container ID; absence cannot be confirmed".into());
    };
    let inspected = docker(&f.host, &["inspect", id])?;
    if inspected.status.success() {
        return Err(format!("teardown did not remove container {id}").into());
    }
    let stderr = String::from_utf8_lossy(&inspected.stderr);
    let absent = stderr.to_ascii_lowercase();
    // The CLI wording varies by version: "No such object" or "no such object".
    if !(absent.contains("no such") || absent.contains("not found")) {
        return Err(format!("teardown could not confirm absence of {id}: {stderr}").into());
    }
    Ok(())
}

fn command(script: &str, timeout: Duration, output_limit_bytes: usize) -> CommandRequest {
    CommandRequest {
        argv: vec![PY.into(), "-c".into(), script.into()],
        timeout,
        output_limit_bytes,
    }
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn docker_post_create_policy_is_verified_against_inspect()
-> Result<(), Box<dyn std::error::Error>> {
    enabled();
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await?;
    let id = f.id.clone().expect("created container id");

    // Independent engine evidence: read the effective host configuration
    // directly rather than trusting the private verifier's own report.
    let scalar = docker(
        &f.host,
        &[
            "inspect",
            "-f",
            "{{.HostConfig.Runtime}}|{{.HostConfig.Memory}}|{{.HostConfig.MemorySwap}}|{{.HostConfig.NanoCpus}}|{{.HostConfig.PidsLimit}}|{{.HostConfig.ReadonlyRootfs}}|{{.HostConfig.NetworkMode}}|{{.HostConfig.Init}}|{{.HostConfig.IpcMode}}|{{.HostConfig.CgroupnsMode}}|{{.HostConfig.OomKillDisable}}|{{.HostConfig.AutoRemove}}|{{.HostConfig.RestartPolicy.Name}}|{{.HostConfig.ShmSize}}",
            &id,
        ],
    )?;
    assert!(
        scalar.status.success(),
        "engine inspect failed: {}",
        String::from_utf8_lossy(&scalar.stderr)
    );
    assert_eq!(
        String::from_utf8(scalar.stdout)?.trim(),
        format!(
            "runc|{MEMORY}|{MEMORY}|100000000|{TEST_PIDS}|true|none|true|private|private|false|false|no|65536"
        )
    );

    let lists = docker(
        &f.host,
        &[
            "inspect",
            "-f",
            "{{json .HostConfig.CapDrop}}|{{json .HostConfig.CapAdd}}|{{json .HostConfig.SecurityOpt}}|{{json .HostConfig.Binds}}|{{json .HostConfig.Mounts}}|{{json .HostConfig.Devices}}|{{json .HostConfig.Tmpfs}}",
            &id,
        ],
    )?;
    assert!(
        lists.status.success(),
        "engine inspect failed: {}",
        String::from_utf8_lossy(&lists.stderr)
    );
    let lists = String::from_utf8(lists.stdout)?;
    let fields: Vec<&str> = lists.trim().split('|').collect();
    assert_eq!(fields.len(), 7, "unexpected inspect output: {lists}");
    assert_eq!(fields[0], r#"["ALL"]"#);
    assert!(fields[1] == "[]" || fields[1] == "null", "{}", fields[1]);
    assert_eq!(fields[2], r#"["no-new-privileges:true"]"#);
    for empty in [fields[3], fields[4], fields[5]] {
        assert!(
            empty == "[]" || empty == "null",
            "unexpected mounts: {empty}"
        );
    }
    let tmpfs = fields[6];
    assert!(tmpfs.contains(r#""/workspace""#), "{tmpfs}");
    for required in ["nosuid", "nodev", "uid=65534", "gid=65534", "size=8388608"] {
        assert!(
            tmpfs.contains(required),
            "tmpfs missing {required}: {tmpfs}"
        );
    }

    // The public status evidence agrees with the engine.
    let status = f.sandbox().status().await?;
    let enforcement = status.enforcement.expect("created container evidence");
    assert_eq!(enforcement.memory_bytes, Some(MEMORY as i64));
    assert_eq!(enforcement.nano_cpus, Some(100_000_000));
    assert_eq!(enforcement.pids_limit, Some(i64::from(TEST_PIDS)));
    assert_eq!(enforcement.readonly_root, Some(true));
    assert_eq!(enforcement.network_mode.as_deref(), Some("none"));
    assert_eq!(enforcement.bind_count, Some(0));
    assert!(
        enforcement
            .cap_drop
            .as_ref()
            .is_some_and(|caps| caps.iter().any(|cap| cap == "ALL"))
    );
    teardown(&mut f).await
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn docker_normal_delete_confirms_absence_and_inspect_fails_after_delete()
-> Result<(), Box<dyn std::error::Error>> {
    enabled();
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await?;
    f.sandbox().start().await?;
    let id = f.id.clone().expect("created container id");
    require_inspect(&f.host, &id)?;

    f.sandbox().destroy().await?;
    let status = f.sandbox().status().await?;
    assert_eq!(status.phase, SandboxPhase::Destroyed);
    assert_eq!(status.instance_id, None);
    f.sandbox().destroy().await?;
    assert!(
        !inspect_exists(&f.host, &id)?,
        "container {id} still exists"
    );
    teardown(&mut f).await
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn docker_missing_container_is_treated_as_confirmed_absence()
-> Result<(), Box<dyn std::error::Error>> {
    enabled();
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await?;
    let id = f.id.clone().expect("created container id");

    // Remove out of band. The sealed handle must not report a false failure
    // (or a false success before checking): destroy observes absence.
    let removed = docker(&f.host, &["rm", "-f", &id])?;
    assert!(
        removed.status.success(),
        "out-of-band removal failed: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!inspect_exists(&f.host, &id)?);

    let sandbox = f.sandbox();
    sandbox.destroy().await?;
    let status = sandbox.status().await?;
    assert_eq!(status.phase, SandboxPhase::Destroyed);
    assert_eq!(status.instance_id, None);
    // Repeated destroy after confirmed absence stays successful and does not
    // invent a new backend instance.
    sandbox.destroy().await?;
    assert!(!inspect_exists(&f.host, &id)?);
    teardown(&mut f).await
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn docker_aggregate_output_budget_allows_exact_limit_and_reclaims_on_overflow()
-> Result<(), Box<dyn std::error::Error>> {
    enabled();
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await?;
    f.sandbox().start().await?;
    let id = f.id.clone().expect("created container id");

    // Exactly 64 bytes across both streams is accepted.
    let exact = f
        .sandbox()
        .exec(command(
            "import sys; sys.stdout.write('a'*32); sys.stdout.flush(); sys.stderr.write('b'*32); sys.stderr.flush()",
            Duration::from_secs(5),
            64,
        ))
        .await?;
    assert_eq!(exact.exit_code, 0);
    assert_eq!(exact.stdout.len(), 32);
    assert_eq!(exact.stderr.len(), 32);

    // 40 stdout + 40 stderr crosses the shared budget on stderr even though
    // each stream alone is under 64; the whole container must be reclaimed.
    let error = f
        .sandbox()
        .exec(command(
            "import subprocess, sys, time; subprocess.Popen(['sleep','300']); sys.stdout.write('a'*40); sys.stdout.flush(); time.sleep(0.2); sys.stderr.write('b'*40); sys.stderr.flush(); time.sleep(300)",
            Duration::from_secs(5),
            64,
        ))
        .await;
    match error {
        Err(SandboxError::OutputLimit {
            limit,
            phase,
            ownership,
            ..
        }) => {
            assert_eq!(limit, 64);
            assert_eq!(phase, OutputPhase::Stderr);
            assert_eq!(ownership, Ownership::ConfirmedAbsent);
        }
        other => panic!("unexpected output-limit result: {other:?}"),
    }
    assert_eq!(f.sandbox().status().await?.phase, SandboxPhase::Destroyed);
    assert!(
        !inspect_exists(&f.host, &id)?,
        "container and descendants were not reclaimed"
    );
    teardown(&mut f).await
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn docker_slow_command_timeout_destroys_and_leaves_no_residue()
-> Result<(), Box<dyn std::error::Error>> {
    enabled();
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await?;
    f.sandbox().start().await?;
    let id = f.id.clone().expect("created container id");

    let error = f
        .sandbox()
        .exec(command(
            "import time; open('/workspace/started','w').write('yes'); time.sleep(30)",
            Duration::from_secs(1),
            4096,
        ))
        .await;
    match error {
        Err(SandboxError::TimedOut {
            operation,
            ownership,
            ..
        }) => {
            assert_eq!(operation, SandboxOperation::Exec);
            assert_eq!(ownership, Ownership::ConfirmedAbsent);
        }
        other => panic!("unexpected timeout result: {other:?}"),
    }
    assert_eq!(f.sandbox().status().await?.phase, SandboxPhase::Destroyed);
    assert!(
        !inspect_exists(&f.host, &id)?,
        "timed-out command left its container behind"
    );
    teardown(&mut f).await
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn docker_status_timeout_does_not_destroy_the_instance()
-> Result<(), Box<dyn std::error::Error>> {
    enabled();
    let _serial = SERIAL.lock().await;
    let mut f =
        Fixture::with_config(backend()?, config_with_lifecycle(Duration::from_nanos(1))?).await?;
    let id = f.id.clone().expect("created container id");

    // A one-nanosecond deadline can still race with an immediately ready
    // inspection, so probe until the deadline actually fires. Whatever the
    // outcome, the read-only status call must never destroy the instance.
    let mut timed_out = false;
    for _ in 0..64 {
        match f.sandbox().status().await {
            Err(SandboxError::TimedOut {
                instance_id,
                ownership,
                operation,
                ..
            }) => {
                // `id` came from the Docker CLI's short-ID listing; the domain
                // reports the engine's full 64-character identity.
                assert!(
                    instance_id
                        .as_deref()
                        .is_some_and(|full| full.starts_with(id.as_str())),
                    "reported {instance_id:?} does not match owned container {id}"
                );
                assert_eq!(ownership, Ownership::StillPresent);
                assert_eq!(operation, SandboxOperation::Status);
                timed_out = true;
                break;
            }
            Ok(status) => assert_eq!(status.phase, SandboxPhase::Created),
            other => panic!("unexpected status result: {other:?}"),
        }
    }
    assert!(
        timed_out,
        "a one-nanosecond status deadline never fired in 64 attempts"
    );

    // The read-only inspection deadline must not have removed the container.
    require_inspect(&f.host, &id)?;
    teardown(&mut f).await
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn docker_cancelled_create_retains_owned_cleanup() -> Result<(), Box<dyn std::error::Error>> {
    enabled();
    let _serial = SERIAL.lock().await;
    let backend = backend()?;
    let f_host = host(&backend);
    let before = owned_container_count(&f_host)?;

    let mut observed_owned = false;
    for _ in 0..3 {
        let task = tokio::spawn(Sandbox::create(backend.clone(), config()?));
        for _ in 0..250 {
            tokio::time::sleep(Duration::from_millis(2)).await;
            if owned_container_count(&f_host)? > before {
                observed_owned = true;
                break;
            }
            if task.is_finished() {
                break;
            }
        }
        task.abort();
        // Dropping the create future or a completed Sandbox retains worker
        // ownership; the worker must reclaim the labelled container. A create
        // that finished before the abort is destroyed explicitly.
        match task.await {
            Ok(Ok(sandbox)) => sandbox.destroy().await?,
            Ok(Err(error)) => return Err(error.into()),
            Err(error) => {
                if !error.is_cancelled() {
                    return Err(error.into());
                }
            }
        }
        let mut reclaimed = false;
        for _ in 0..500 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if owned_container_count(&f_host)? <= before {
                reclaimed = true;
                break;
            }
        }
        assert!(
            reclaimed,
            "cancelled create left an owned container behind (observed_owned={observed_owned})"
        );
        if observed_owned {
            break;
        }
    }
    assert!(
        observed_owned,
        "could not cancel create while an owned container existed; no cleanup path was exercised"
    );
    Ok(())
}
