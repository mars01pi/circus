use std::time::Duration;

use circus_sandbox::environment::{
    EnvironmentPlan, ImagePlatform, ImageRequest, PinnedVersion, PrepareStep, TenantDomain,
};
use circus_sandbox::{IoLimits, IsolationConfig, ResourceLimits, SandboxConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let reference =
        std::env::var("CIRCUS_DOCKER_IMAGE").unwrap_or_else(|_| "python:3.12-slim".into());
    let platform = ImagePlatform::new("linux", std::env::var("CIRCUS_DOCKER_ARCH")?)?;
    let python_pin = std::env::var("CIRCUS_PYTHON_VERSION").unwrap_or_else(|_| "3".into());
    let plan = EnvironmentPlan::new(
        ImageRequest::new(
            std::env::var("CIRCUS_DOCKER_ENDPOINT")?,
            reference,
            platform,
            Duration::from_secs(120),
        )?,
        vec![
            PrepareStep::new(
                vec!["python3".into(), "--version".into()],
                PinnedVersion::new(python_pin)?,
                Duration::from_secs(30),
                4096,
            )?,
            PrepareStep::new(
                vec![
                    "python3".into(),
                    "-c".into(),
                    "print('environment-ready')".into(),
                ],
                PinnedVersion::new("3")?,
                Duration::from_secs(30),
                4096,
            )?,
        ],
    )?;
    // Resolution is local-only; a missing image is typed and never pulled.
    let prepared = plan
        .execute(SandboxConfig::new(
            ResourceLimits::new(64 * 1024 * 1024, 100, 32, 8 * 1024 * 1024, 1024)?,
            IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)?,
            IoLimits::new(64 * 1024, Duration::from_secs(10))?,
        )?)
        .await?;

    println!("image id: {}", prepared.image().id());
    for observation in prepared.observations() {
        println!(
            "step {} exit {} stdout {}",
            observation.index(),
            observation.exit_code(),
            String::from_utf8_lossy(observation.stdout()).trim_end()
        );
    }
    let tenant = TenantDomain::new(
        std::env::var("CIRCUS_TENANT").unwrap_or_else(|_| "example-tenant".into()),
    )?;
    println!("identity: {}", prepared.identity(tenant)?);
    // execute() destroys the sandbox and waits for confirmed instance/staging cleanup.
    println!("cleanup: owned sandbox and staging confirmed absent");
    Ok(())
}
