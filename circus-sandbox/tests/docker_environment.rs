use std::process::Command;
use std::time::Duration;

use circus_sandbox::environment::{
    EnvironmentError, EnvironmentId, EnvironmentPlan, ImagePlatform, ImageRequest, PinnedVersion,
    PlanError, PrepareStep, TenantDomain, prepare_docker_image,
};
use circus_sandbox::{
    BackendConfig, CommandRequest, IoLimits, IsolationConfig, ResourceLimits, Sandbox,
    SandboxConfig, SandboxPhase,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn endpoint() -> TestResult<String> {
    assert_eq!(std::env::var("CIRCUS_DOCKER_SANDBOX").as_deref(), Ok("1"));
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

fn host(endpoint: &str) -> String {
    if endpoint.starts_with('/') {
        format!("unix://{endpoint}")
    } else {
        endpoint.to_string()
    }
}

/// Requires the engine to report the exact owned container gone after destroy.
/// There is no Drop fallback: an unconfirmed removal fails the test.
fn confirm_absent(host: &str, id: &str) -> TestResult {
    let inspected = Command::new("docker")
        .args(["--host", host, "inspect", id])
        .output()?;
    if inspected.status.success() {
        return Err(format!("container {id} still exists after destroy").into());
    }
    let stderr = String::from_utf8_lossy(&inspected.stderr);
    let absent = stderr.to_ascii_lowercase();
    // The CLI wording varies by version: "No such object" or "no such object".
    if !(absent.contains("no such") || absent.contains("not found")) {
        return Err(format!("could not confirm absence of {id}: {stderr}").into());
    }
    Ok(())
}

fn request(reference: String) -> TestResult<ImageRequest> {
    Ok(ImageRequest::new(
        endpoint()?,
        reference,
        platform()?,
        Duration::from_secs(120),
    )?)
}

fn absent_reference() -> String {
    // Public repository, unique nonexistent tag. Never remove daemon images to create a miss.
    format!(
        "docker.io/library/python:circus-missing-{}",
        uuid::Uuid::new_v4()
    )
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker Engine and local Python image"]
async fn local_image_resolves_immutable_id_and_runs() -> TestResult {
    let prepared = prepare_docker_image(request(local_image())?).await?;
    assert!(prepared.id().starts_with("sha256:"));
    assert_eq!(prepared.id().len(), 71);
    assert!(
        prepared.id()[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    );
    assert_eq!(prepared.os(), "linux");
    assert_eq!(prepared.architecture(), platform()?.architecture());
    let repeated = prepare_docker_image(request(prepared.id().into())?).await?;
    assert_eq!(prepared.id(), repeated.id());
    assert_eq!(prepared.repo_digests(), repeated.repo_digests());

    let config = SandboxConfig::new(
        ResourceLimits::new(64 * 1024 * 1024, 100, 32, 8 * 1024 * 1024, 1024)?,
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)?,
        IoLimits::new(64 * 1024, Duration::from_secs(10))?,
    )?;
    // Sandbox generates a unique owner/name; this test owns exactly this container.
    let endpoint = endpoint()?;
    let host = host(&endpoint);
    let backend = BackendConfig::docker(endpoint, prepared.id(), "runc");
    let sandbox = Sandbox::create(backend, config).await?;
    let id = sandbox.status().await?.instance_id.ok_or("missing id")?;
    let execution: TestResult<_> = async {
        sandbox.start().await?;
        let output = sandbox
            .exec(CommandRequest {
                argv: vec![
                    "python3".into(),
                    "-c".into(),
                    "print('prepared-image')".into(),
                ],
                timeout: Duration::from_secs(10),
                output_limit_bytes: 1024,
            })
            .await?;
        Ok(output)
    }
    .await;
    // Explicit cleanup runs even when start/exec returns an error, before checking output.
    sandbox.destroy().await?;
    assert_eq!(sandbox.status().await?.phase, SandboxPhase::Destroyed);
    confirm_absent(&host, &id)?;
    let output = execution?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"prepared-image\n");
    assert!(output.stderr.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker Engine"]
async fn missing_local_image_never_pulls() -> TestResult {
    let reference = absent_reference();
    let result = prepare_docker_image(request(reference.clone())?).await;
    assert!(
        matches!(result, Err(EnvironmentError::ImageNotFound { reference: found }) if found == reference)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker Engine and local Python image"]
async fn local_platform_mismatch_is_an_error() -> TestResult {
    let actual = prepare_docker_image(request(local_image())?).await?;
    let wrong_arch = if actual.architecture() == "amd64" {
        "arm64"
    } else {
        "amd64"
    };
    let result = prepare_docker_image(ImageRequest::new(
        endpoint()?,
        actual.id(),
        ImagePlatform::new("linux", wrong_arch)?,
        Duration::from_secs(30),
    )?)
    .await;
    assert!(
        matches!(result, Err(EnvironmentError::PlatformMismatch { actual_os, actual_architecture, .. })
        if actual_os == actual.os() && actual_architecture == actual.architecture())
    );
    Ok(())
}

fn unconnected_request() -> TestResult<ImageRequest> {
    Ok(ImageRequest::new(
        "unix:///var/run/docker.sock",
        "python:3.12-slim",
        ImagePlatform::new("linux", "amd64")?,
        Duration::from_secs(10),
    )?)
}

fn pinned_step(version: &str) -> TestResult<PrepareStep> {
    Ok(PrepareStep::new(
        vec!["python3".into(), "--version".into()],
        PinnedVersion::new(version)?,
        Duration::from_secs(10),
        1024,
    )?)
}

#[test]
fn plan_rejects_unpinned_step_before_docker() -> TestResult {
    let unpinned = PrepareStep::unpinned(
        vec!["python3".into(), "--version".into()],
        Duration::from_secs(10),
        1024,
    )?;
    let result = EnvironmentPlan::new(unconnected_request()?, vec![unpinned]);
    assert!(matches!(result, Err(PlanError::UnpinnedInput { index: 0 })));
    Ok(())
}

#[test]
fn identity_is_stable_and_scoped_to_tenant() -> TestResult {
    let platform = ImagePlatform::new("linux", "amd64")?;
    let step = pinned_step("3.12.1")?;
    let first = EnvironmentId::new(
        "sha256:1111",
        vec!["repo@sha256:aaa".into()],
        platform.clone(),
        vec![step.clone()],
        TenantDomain::new("tenant-a")?,
    )?;
    let repeated = EnvironmentId::new(
        "sha256:1111",
        vec!["repo@sha256:aaa".into()],
        platform.clone(),
        vec![step.clone()],
        TenantDomain::new("tenant-a")?,
    )?;
    let other_tenant = EnvironmentId::new(
        "sha256:1111",
        vec!["repo@sha256:aaa".into()],
        platform,
        vec![step],
        TenantDomain::new("tenant-b")?,
    )?;
    assert_eq!(first, repeated);
    assert_ne!(first, other_tenant);
    assert_eq!(first.to_string().len(), 64);
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Linux Docker Engine and local Python image"]
async fn resolved_image_supports_plan_identity() -> TestResult {
    let image = prepare_docker_image(request(local_image())?).await?;
    let plan = EnvironmentPlan::new(request(local_image())?, vec![pinned_step("3")?])?;
    let first = plan.identity(&image, TenantDomain::new("tenant-a")?)?;
    let second = plan.identity(&image, TenantDomain::new("tenant-b")?)?;
    assert_ne!(first, second);
    assert_eq!(first.to_string().len(), 64);
    assert!(
        first
            .to_string()
            .bytes()
            .all(|byte| { byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte) })
    );
    Ok(())
}
