use std::collections::{HashMap, HashSet};
use std::time::Duration;

use circus_sandbox::environment::{
    EnvironmentId, EnvironmentPlan, IdentityError, ImagePlatform, ImageRequest, PinnedVersion,
    PlanError, PreparationError, PrepareStep, TenantDomain,
};
use circus_sandbox::{IoLimits, IsolationConfig, ResourceLimits, SandboxConfig};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const WAIT: Duration = Duration::from_secs(60);
const OWNER_LABEL_KEY: &str = "circus.sandbox.owner";
// Serialize this binary's before/after owner inventories.
static DOCKER_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn enabled() {
    assert_eq!(
        std::env::var("CIRCUS_DOCKER_SANDBOX").as_deref(),
        Ok("1"),
        "set CIRCUS_DOCKER_SANDBOX=1 to opt into the real backend tests"
    );
}

fn endpoint() -> TestResult<String> {
    Ok(std::env::var("CIRCUS_DOCKER_ENDPOINT")?)
}

fn platform() -> TestResult<ImagePlatform> {
    Ok(ImagePlatform::new(
        "linux",
        std::env::var("CIRCUS_DOCKER_ARCH")?,
    )?)
}

fn local_image() -> String {
    std::env::var("CIRCUS_DOCKER_IMAGE").unwrap_or_else(|_| "python:3.12-slim".into())
}

fn request() -> TestResult<ImageRequest> {
    Ok(ImageRequest::new(
        endpoint()?,
        local_image(),
        platform()?,
        Duration::from_secs(120),
    )?)
}

fn config() -> TestResult<SandboxConfig> {
    Ok(SandboxConfig::new(
        ResourceLimits::new(64 * 1024 * 1024, 100, 32, 8 * 1024 * 1024, 1024)?,
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)?,
        IoLimits::new(64 * 1024, Duration::from_secs(10))?,
    )?)
}

fn docker() -> TestResult<bollard::Docker> {
    let endpoint = endpoint()?;
    let path = if endpoint.starts_with('/') {
        format!("unix://{endpoint}")
    } else {
        endpoint
    };
    Ok(bollard::Docker::connect_with_socket(
        &path,
        30,
        bollard::API_DEFAULT_VERSION,
    )?)
}

async fn owned_ids(docker: &bollard::Docker) -> TestResult<HashSet<String>> {
    let containers = docker
        .list_containers(Some(bollard::query_parameters::ListContainersOptions {
            all: true,
            filters: Some(HashMap::from([(
                "label".to_string(),
                vec![OWNER_LABEL_KEY.to_string()],
            )])),
            ..Default::default()
        }))
        .await?;
    Ok(containers
        .into_iter()
        .filter_map(|container| container.id)
        .collect())
}

async fn wait_new_owned(docker: &bollard::Docker, before: &HashSet<String>) -> TestResult<String> {
    loop {
        let current = owned_ids(docker).await?;
        if let Some(id) = current.difference(before).next() {
            return Ok(id.clone());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_running(docker: &bollard::Docker, id: &str) -> TestResult {
    loop {
        match docker.inspect_container(id, None).await {
            Ok(info) => {
                let running = info
                    .state
                    .as_ref()
                    .and_then(|state| state.running)
                    .unwrap_or(false);
                if running {
                    return Ok(());
                }
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                return Err("container disappeared before it started".to_string().into());
            }
            Err(error) => return Err(error.into()),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_absent_owned(docker: &bollard::Docker, id: &str) -> TestResult {
    loop {
        let current = owned_ids(docker).await?;
        if !current.contains(id) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn step(argv: &[&str], version: &str, output_limit_bytes: usize) -> TestResult<PrepareStep> {
    Ok(PrepareStep::new(
        argv.iter().map(|value| (*value).to_string()).collect(),
        PinnedVersion::new(version)?,
        Duration::from_secs(30),
        output_limit_bytes,
    )?)
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker Engine and local Python image"]
async fn pinned_preparation_records_exact_output_and_releases() -> TestResult {
    enabled();
    let _guard = DOCKER_TEST.lock().await;
    let docker = docker()?;
    let before = owned_ids(&docker).await?;

    let plan = EnvironmentPlan::new(
        request()?,
        vec![
            step(
                &[
                    "python3",
                    "-c",
                    "import sys; sys.stdout.write('prepared\\n')",
                ],
                "3",
                4096,
            )?,
            step(&["python3", "--version"], "3", 4096)?,
        ],
    )?;
    let prepared = plan.execute(config()?).await?;

    assert!(!prepared.image().id().is_empty());
    assert_eq!(prepared.platform().clone(), platform()?);
    assert_eq!(prepared.observations()[0].index(), 0);
    assert_eq!(
        prepared.observations()[0].stdout(),
        b"prepared\n".as_slice()
    );
    assert_eq!(prepared.observations()[0].stderr(), b"".as_slice());
    assert!(
        prepared.observations()[1]
            .stdout()
            .starts_with(b"Python 3.")
    );
    assert_eq!(
        prepared.observations()[0]
            .argv()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec![
            "python3",
            "-c",
            "import sys; sys.stdout.write('prepared\\n')"
        ]
    );

    let identity = prepared.identity(TenantDomain::new("tenant-a")?)?;
    assert_eq!(identity.to_string().len(), 64);
    assert_ne!(identity, prepared.identity(TenantDomain::new("tenant-b")?)?);

    let after = owned_ids(&docker).await?;
    assert!(
        after.is_subset(&before),
        "prepared container was not released: {after:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker Engine and local Python image"]
async fn failing_step_reports_index_exit_and_releases() -> TestResult {
    enabled();
    let _guard = DOCKER_TEST.lock().await;
    let docker = docker()?;
    let before = owned_ids(&docker).await?;

    let failing = "import sys; sys.stdout.write('out'); sys.stderr.write('boom'); sys.exit(7)";
    let plan = EnvironmentPlan::new(
        request()?,
        vec![
            step(&["python3", "-c", "pass"], "3", 2048)?,
            step(&["python3", "-c", failing], "3", 2048)?,
        ],
    )?;
    let error = plan
        .execute(config()?)
        .await
        .expect_err("the second step exits non-zero");
    match error {
        PreparationError::StepFailed {
            index,
            argv,
            exit_code,
            stdout,
            stderr,
            output_limit_bytes,
            cleanup,
        } => {
            assert_eq!(index, 1);
            assert_eq!(
                argv,
                vec!["python3".to_string(), "-c".to_string(), failing.to_string()]
            );
            assert_eq!(exit_code, 7);
            assert_eq!(stdout, b"out");
            assert_eq!(output_limit_bytes, 2048);
            assert_eq!(stderr, b"boom");
            assert_eq!(cleanup, None, "cleanup must be confirmed");
        }
        other => panic!("unexpected preparation error: {other:?}"),
    }

    let after = owned_ids(&docker).await?;
    assert!(
        after.is_subset(&before),
        "failed preparation left a container: {after:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker Engine and local Python image"]
async fn cancelled_preparation_releases_container() -> TestResult {
    enabled();
    let _guard = DOCKER_TEST.lock().await;
    let docker = docker()?;
    let before = owned_ids(&docker).await?;

    let plan = EnvironmentPlan::new(
        request()?,
        vec![step(
            &["python3", "-c", "import time; time.sleep(30)"],
            "3",
            1024,
        )?],
    )?;
    let config = config()?;
    let handle = tokio::spawn(async move { plan.execute(config).await });

    let id = tokio::time::timeout(WAIT, wait_new_owned(&docker, &before)).await??;
    tokio::time::timeout(WAIT, wait_running(&docker, &id)).await??;
    // Give the exec request time to become the active operation before cancelling.
    tokio::time::sleep(Duration::from_millis(300)).await;
    handle.abort();
    match handle.await {
        Ok(_) => return Err("preparation completed instead of being cancelled".into()),
        Err(error) => {
            if !error.is_cancelled() {
                return Err(error.into());
            }
        }
    }

    tokio::time::timeout(WAIT, wait_absent_owned(&docker, &id)).await??;
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker Engine"]
async fn unpinned_step_is_rejected_before_allocation() -> TestResult {
    enabled();
    let _guard = DOCKER_TEST.lock().await;
    let docker = docker()?;
    let before = owned_ids(&docker).await?;

    let unpinned = PrepareStep::unpinned(
        vec!["python3".into(), "--version".into()],
        Duration::from_secs(30),
        1024,
    )?;
    let result = EnvironmentPlan::new(request()?, vec![unpinned]);
    assert!(matches!(result, Err(PlanError::UnpinnedInput { index: 0 })));

    let after = owned_ids(&docker).await?;
    assert!(
        after.is_subset(&before),
        "plan validation allocated a container: {after:?}"
    );
    Ok(())
}

fn identity_step(program: &str, version: &str) -> TestResult<PrepareStep> {
    Ok(PrepareStep::new(
        vec![program.to_string(), "--version".to_string()],
        PinnedVersion::new(version)?,
        Duration::from_secs(5),
        1024,
    )?)
}

#[test]
fn identity_is_stable_and_sensitive_to_every_input() -> TestResult {
    let platform = ImagePlatform::new("linux", "amd64")?;
    let base = EnvironmentId::new(
        "sha256:1111",
        vec!["repo@sha256:aaa".into(), "repo@sha256:bbb".into()],
        platform.clone(),
        vec![identity_step("python3", "3.12.1")?],
        TenantDomain::new("tenant-a")?,
    )?;
    let reversed_digests = EnvironmentId::new(
        "sha256:1111",
        vec!["repo@sha256:bbb".into(), "repo@sha256:aaa".into()],
        platform.clone(),
        vec![identity_step("python3", "3.12.1")?],
        TenantDomain::new("tenant-a")?,
    )?;
    assert_eq!(base, reversed_digests);

    let changed = [
        EnvironmentId::new(
            "sha256:2222",
            vec!["repo@sha256:aaa".into(), "repo@sha256:bbb".into()],
            platform.clone(),
            vec![identity_step("python3", "3.12.1")?],
            TenantDomain::new("tenant-a")?,
        )?,
        EnvironmentId::new(
            "sha256:1111",
            vec!["repo@sha256:aaa".into(), "repo@sha256:ccc".into()],
            platform.clone(),
            vec![identity_step("python3", "3.12.1")?],
            TenantDomain::new("tenant-a")?,
        )?,
        EnvironmentId::new(
            "sha256:1111",
            vec!["repo@sha256:aaa".into(), "repo@sha256:bbb".into()],
            platform.clone(),
            vec![identity_step("python3", "3.12.2")?],
            TenantDomain::new("tenant-a")?,
        )?,
        EnvironmentId::new(
            "sha256:1111",
            vec!["repo@sha256:aaa".into(), "repo@sha256:bbb".into()],
            platform.clone(),
            vec![identity_step("python3", "3.12.1")?],
            TenantDomain::new("tenant-b")?,
        )?,
        EnvironmentId::new(
            "sha256:1111",
            vec!["repo@sha256:aaa".into(), "repo@sha256:bbb".into()],
            ImagePlatform::with_variant("linux", "amd64", Some("v3".into()))?,
            vec![identity_step("python3", "3.12.1")?],
            TenantDomain::new("tenant-a")?,
        )?,
    ];
    for value in changed {
        assert_ne!(base, value);
    }
    Ok(())
}

#[test]
fn identity_rejects_unpinned_and_duplicate_inputs() -> TestResult {
    let platform = ImagePlatform::new("linux", "amd64")?;
    let tenant = TenantDomain::new("tenant-a")?;
    let unpinned = PrepareStep::unpinned(
        vec!["python3".into(), "--version".into()],
        Duration::from_secs(5),
        1024,
    )?;
    assert_eq!(
        EnvironmentId::new(
            "sha256:1111",
            Vec::new(),
            platform.clone(),
            vec![unpinned],
            tenant.clone(),
        ),
        Err(IdentityError::UnpinnedInput { index: 0 })
    );
    assert_eq!(
        EnvironmentId::new(
            "sha256:1111",
            vec!["repo@sha256:aaa".into(), "repo@sha256:aaa".into()],
            platform,
            vec![identity_step("python3", "3.12.1")?],
            tenant,
        ),
        Err(IdentityError::DuplicateRepoDigest { index: 1 })
    );
    Ok(())
}

#[test]
fn identity_display_is_opaque_hex() -> TestResult {
    let value = EnvironmentId::new(
        "sha256:1111",
        vec!["repo@sha256:aaa".into()],
        ImagePlatform::new("linux", "amd64")?,
        vec![identity_step("/usr/local/bin/python3", "3.12.1")?],
        TenantDomain::new("tenant-a")?,
    )?;
    let display = value.to_string();
    assert_eq!(display.len(), 64);
    assert!(
        display
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    let debug = format!("{value:?}");
    assert!(!debug.contains("python"));
    assert!(!debug.contains("tenant"));
    assert!(!display.contains("sha256"));
    Ok(())
}
