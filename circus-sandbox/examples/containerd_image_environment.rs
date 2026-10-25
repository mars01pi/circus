#[cfg(unix)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;

    use circus_sandbox::environment::{
        ContainerdImageRequest, ImagePlatform, ImagePullPolicy, prepare_containerd_image,
    };

    if std::env::var("CIRCUS_CONTAINERD").as_deref() != Ok("1") {
        return Err("set CIRCUS_CONTAINERD=1 to explicitly authorize daemon access".into());
    }
    let socket = std::env::var("CIRCUS_CONTAINERD_SOCKET")?;
    let namespace = std::env::var("CIRCUS_CONTAINERD_NAMESPACE")?;
    let reference = std::env::var("CIRCUS_CONTAINERD_IMAGE")?;
    let variant = match std::env::var("CIRCUS_CONTAINERD_VARIANT") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(error.into()),
    };
    let platform = ImagePlatform::with_variant(
        std::env::var("CIRCUS_CONTAINERD_OS")?,
        std::env::var("CIRCUS_CONTAINERD_ARCH")?,
        variant,
    )?;
    let timeout = Duration::from_secs(std::env::var("CIRCUS_CONTAINERD_TIMEOUT_SECS")?.parse()?);
    let policy = match std::env::var("CIRCUS_CONTAINERD_PULL_POLICY")?.as_str() {
        "local-only" => ImagePullPolicy::LocalOnly,
        "if-missing" => ImagePullPolicy::IfMissing,
        _ => return Err("CIRCUS_CONTAINERD_PULL_POLICY must be local-only or if-missing".into()),
    };
    let request = ContainerdImageRequest::new(socket, namespace, reference, platform, timeout)?;
    let prepared = prepare_containerd_image(request.clone().with_pull_policy(policy)).await?;
    println!(
        "prepared: {} {} {:?}",
        prepared.reference(),
        prepared.digest(),
        prepared.platform()
    );

    // Reuse the source record so index-only variant evidence remains available.
    let cached =
        prepare_containerd_image(request.with_pull_policy(ImagePullPolicy::LocalOnly)).await?;
    if cached != prepared {
        return Err("local-only repeat returned different image metadata".into());
    }
    println!(
        "local-only cache hit: {} {} {:?}",
        cached.reference(),
        cached.digest(),
        cached.platform()
    );
    Ok(())
}

#[cfg(not(unix))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    Err("containerd image preparation requires a Unix host and an explicit local socket".into())
}
