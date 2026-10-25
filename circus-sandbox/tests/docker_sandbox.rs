use std::process::Command;
use std::time::Duration;

use circus_sandbox::{
    BackendConfig, CommandError, CommandRequest, IoLimits, IsolationConfig, ResourceLimits,
    Sandbox, SandboxConfig, SandboxError, SandboxPhase, StagingError,
};

const PY: &str = "python3";
// OCI transient helper overhead measured under 8 processes requires fixture headroom.
const TEST_PIDS: u32 = 32;

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
    let limits = ResourceLimits::new(64 * 1024 * 1024, 100, TEST_PIDS, 8 * 1024 * 1024, 1024)?;
    Ok(SandboxConfig::new(
        limits,
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)?,
        IoLimits::new(64 * 1024, Duration::from_secs(3))?,
    )?)
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
        let sandbox = Sandbox::create(backend, config).await?;
        let id = sandbox.status().await?.instance_id;
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
    let inspected = Command::new("docker")
        .args(["--host", &f.host, "inspect", id])
        .output()?;
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

fn py(argv: &[&str]) -> CommandRequest {
    CommandRequest {
        argv: std::iter::once(PY.to_string())
            .chain(argv.iter().map(|s| (*s).into()))
            .collect(),
        timeout: Duration::from_secs(5),
        output_limit_bytes: 16 * 1024,
    }
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn docker_linux_runc_lifecycle() -> Result<(), Box<dyn std::error::Error>> {
    enabled();
    let mut f = Fixture::new().await?;
    f.sandbox().write_file("hello.txt", b"hello").await?;
    f.sandbox().create_dir_all("staged").await?;
    f.sandbox().start().await?;
    let status = f.sandbox().status().await?;
    assert_eq!(status.phase, SandboxPhase::Running);
    let observed = status
        .enforcement
        .as_ref()
        .expect("running sandbox exposes inspected evidence");
    assert_eq!(observed.runtime.as_deref(), Some("runc"));
    assert_eq!(observed.memory_bytes, Some(64 * 1024 * 1024));
    assert_eq!(observed.nano_cpus, Some(100_000_000));
    assert_eq!(observed.pids_limit, Some(i64::from(TEST_PIDS)));
    assert_eq!(observed.readonly_root, Some(true));
    assert_eq!(observed.network_mode.as_deref(), Some("none"));
    assert_eq!(observed.bind_count, Some(0));
    assert!(
        observed
            .cap_drop
            .as_ref()
            .is_some_and(|caps| caps.iter().any(|cap| cap == "ALL"))
    );
    let security = f.sandbox().exec(py(&["-c", r###"from pathlib import Path; s=Path('/proc/self/status').read_text(); print(next(x for x in s.splitlines() if x.startswith('NoNewPrivs:'))); print(next(x for x in s.splitlines() if x.startswith('CapEff:')))"###])).await?;
    let security_text = String::from_utf8(security.stdout)?;
    assert!(
        security_text
            .lines()
            .any(|line| line.contains("NoNewPrivs:") && line.ends_with("1"))
    );
    assert!(
        security_text
            .lines()
            .any(|line| line.contains("CapEff:") && line.ends_with("0000000000000000"))
    );
    let limits = f.sandbox().exec(py(&["-c", r###"from pathlib import Path; root=Path('/sys/fs/cgroup'); print((root/'memory.max').read_text().strip()); print((root/'pids.max').read_text().strip()); print((root/'cpu.max').read_text().strip())"###])).await?;
    let limit_text = String::from_utf8(limits.stdout)?;
    assert_eq!(limit_text.lines().next(), Some("67108864"));
    assert_eq!(limit_text.lines().nth(1), Some("32"));
    assert_eq!(limit_text.lines().nth(2), Some("10000 100000"));
    let out = f.sandbox().exec(py(&["-c", r###"from pathlib import Path; Path('/workspace/hello.txt').write_bytes(b'changed'); Path('/workspace/staged/guest.txt').write_bytes(b'guest')"###])).await?;
    assert_eq!(out.exit_code, 0);
    assert_eq!(f.sandbox().read_file("hello.txt").await?, b"changed");
    f.sandbox()
        .exec(py(&[
            "-c",
            r###"from pathlib import Path; Path('/workspace/newdir').mkdir()"###,
        ]))
        .await?;
    f.sandbox().write_file("newdir/sdk.txt", b"sdk").await?;
    f.sandbox().pause().await?;
    assert_eq!(f.sandbox().status().await?.phase, SandboxPhase::Paused);
    assert!(matches!(
        f.sandbox().write_file("paused", b"x").await,
        Err(SandboxError::InvalidPhase { .. })
    ));
    f.sandbox().resume().await?;
    f.sandbox().remove_file("staged/guest.txt").await?;
    f.sandbox().destroy().await?;
    assert_eq!(f.sandbox().status().await?.phase, SandboxPhase::Destroyed);
    f.sandbox().destroy().await?;
    teardown(&mut f).await
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn docker_linux_runc_ownership_is_per_create() -> Result<(), Box<dyn std::error::Error>> {
    enabled();
    let mut first = Fixture::new().await?;
    let mut second = Fixture::new().await?;
    assert_ne!(first.id, second.id);
    first.sandbox().start().await?;
    second.sandbox().start().await?;
    teardown(&mut second).await?;
    assert_eq!(first.sandbox().status().await?.phase, SandboxPhase::Running);
    teardown(&mut first).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn docker_linux_runc_guest_paths_and_file_limits() -> Result<(), Box<dyn std::error::Error>> {
    enabled();
    let mut f = Fixture::new().await?;
    f.sandbox().write_file("secret", b"secret").await?;
    f.sandbox().start().await?;
    f.sandbox().exec(py(&["-c", r###"import os, pathlib; pathlib.Path('/workspace/link').symlink_to('/workspace/secret'); pathlib.Path('/workspace/escape').symlink_to('/etc'); os.mkfifo('/workspace/pipe')"###])).await?;
    let link_read =
        tokio::time::timeout(Duration::from_secs(3), f.sandbox().read_file("link")).await?;
    assert!(link_read.is_err());
    let pipe_write =
        tokio::time::timeout(Duration::from_secs(3), f.sandbox().write_file("pipe", b"x")).await?;
    assert!(pipe_write.is_err());
    let escape_read = tokio::time::timeout(
        Duration::from_secs(3),
        f.sandbox().read_file("escape/passwd"),
    )
    .await?;
    assert!(escape_read.is_err());
    assert_eq!(f.sandbox().read_file("secret").await?, b"secret");
    assert!(f.sandbox().write_file("missing/file", b"x").await.is_err());
    f.sandbox().create_dir_all("missing").await?;
    f.sandbox().write_file("missing/file", b"x").await?;
    f.sandbox()
        .exec(py(&[
            "-c",
            r###"open('/workspace/too-big','wb').write(b'x'*2048)"###,
        ]))
        .await?;
    let too_big = f.sandbox().read_file("too-big").await;
    assert!(too_big.is_err());
    assert!(f.sandbox().read_file("secret").await? == b"secret");
    teardown(&mut f).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn docker_linux_runc_exec_abort_destroys_only_owner() -> Result<(), Box<dyn std::error::Error>>
{
    enabled();
    let mut victim = Fixture::new().await?;
    let mut survivor = Fixture::new().await?;
    victim.sandbox().start().await?;
    survivor.sandbox().start().await?;
    let request = CommandRequest {
        argv: vec![
            PY.into(),
            "-c".into(),
            "import time; open('/workspace/started','w').write('yes'); time.sleep(30)".into(),
        ],
        timeout: Duration::from_secs(1),
        output_limit_bytes: 4096,
    };
    let err = victim.sandbox().exec(request).await;
    assert!(matches!(
        err,
        Err(SandboxError::TimedOut {
            ownership: circus_sandbox::Ownership::ConfirmedAbsent,
            ..
        })
    ));
    assert_eq!(
        victim.sandbox().status().await?.phase,
        SandboxPhase::Destroyed
    );
    let out = survivor
        .sandbox()
        .exec(py(&["-c", r###"print('alive')"###]))
        .await?;
    assert_eq!(out.stdout, b"alive\n");
    let output_err = survivor
        .sandbox()
        .exec(CommandRequest {
            argv: vec![PY.into(), "-c".into(), "print('x'*10000)".into()],
            timeout: Duration::from_secs(5),
            output_limit_bytes: 64,
        })
        .await;
    assert!(matches!(
        output_err,
        Err(SandboxError::OutputLimit {
            ownership: circus_sandbox::Ownership::ConfirmedAbsent,
            ..
        })
    ));
    assert_eq!(
        survivor.sandbox().status().await?.phase,
        SandboxPhase::Destroyed
    );
    teardown(&mut victim).await?;
    teardown(&mut survivor).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn docker_linux_runc_invalid_commands_preserve_sandbox()
-> Result<(), Box<dyn std::error::Error>> {
    enabled();
    let mut f = Fixture::new().await?;
    f.sandbox().start().await?;
    let mut empty_argv = py(&["-c", "print('unexpected')"]);
    empty_argv.argv.clear();
    let mut zero_timeout = py(&["-c", "print('unexpected')"]);
    zero_timeout.timeout = Duration::ZERO;
    let mut zero_limit = py(&["-c", "print('unexpected')"]);
    zero_limit.output_limit_bytes = 0;
    for (request, expected) in [
        (empty_argv, CommandError::EmptyArgv),
        (
            zero_timeout,
            CommandError::InvalidTimeout {
                timeout: Duration::ZERO,
            },
        ),
        (
            zero_limit,
            CommandError::InvalidOutputLimit {
                output_limit_bytes: 0,
            },
        ),
    ] {
        let err = f.sandbox().exec(request).await.unwrap_err();
        assert!(
            !matches!(err, SandboxError::RelativePath { .. }),
            "invalid command was classified as a path failure: {err:?}"
        );
        assert!(matches!(&err, SandboxError::Command(actual) if actual == &expected));
        assert_eq!(
            std::error::Error::source(&err)
                .and_then(|source| source.downcast_ref::<CommandError>()),
            Some(&expected)
        );
        let status = f.sandbox().status().await?;
        assert_eq!(status.phase, SandboxPhase::Running);
        assert_eq!(status.instance_id, f.id);
        let out = f.sandbox().exec(py(&["-c", "print('alive')"])).await?;
        assert_eq!(out.stdout, b"alive\n");
    }
    teardown(&mut f).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Linux Docker Engine"]
async fn docker_linux_runc_staging_limit_is_not_a_path_error()
-> Result<(), Box<dyn std::error::Error>> {
    enabled();
    let config = SandboxConfig::new(
        ResourceLimits::new(64 * 1024 * 1024, 100, TEST_PIDS, 1024, 1024)?,
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)?,
        IoLimits::new(64 * 1024, Duration::from_secs(3))?,
    )?;
    let mut f = Fixture::with_config(backend()?, config).await?;
    let bytes = vec![b'x'; 700];
    f.sandbox().write_file("first", &bytes).await?;
    f.sandbox().write_file("second", &bytes).await?;
    f.sandbox().write_file("third", &bytes).await?;
    let err = f.sandbox().start().await.unwrap_err();
    assert!(
        !matches!(err, SandboxError::RelativePath { .. }),
        "staging bound was classified as a path failure: {err:?}"
    );
    let expected = StagingError::LimitExceeded {
        limit: 1024,
        actual: 1400,
    };
    assert!(matches!(&err, SandboxError::Staging(actual) if actual == &expected));
    assert_eq!(
        std::error::Error::source(&err).and_then(|source| source.downcast_ref::<StagingError>()),
        Some(&expected)
    );
    let status = f.sandbox().status().await?;
    assert_eq!(status.phase, SandboxPhase::Created);
    assert_eq!(status.instance_id, f.id);
    assert_eq!(f.sandbox().read_file("first").await?, bytes);
    assert_eq!(f.sandbox().read_file("second").await?, bytes);
    assert_eq!(f.sandbox().read_file("third").await?, bytes);
    f.sandbox().remove_file("first").await?;
    f.sandbox().remove_file("third").await?;
    f.sandbox().write_file("second", &vec![b'x'; 1024]).await?;
    f.sandbox().start().await?;
    assert_eq!(f.sandbox().read_file("second").await?, vec![b'x'; 1024]);
    teardown(&mut f).await?;
    Ok(())
}
