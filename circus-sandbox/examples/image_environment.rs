use std::time::Duration;

use circus_sandbox::environment::{
    ImagePlatform, ImagePullPolicy, ImageRequest, prepare_docker_image,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let reference =
        std::env::var("CIRCUS_DOCKER_IMAGE").unwrap_or_else(|_| "python:3.12-slim".into());
    let platform = ImagePlatform::new("linux", std::env::var("CIRCUS_DOCKER_ARCH")?)?;
    let pull_policy = match std::env::var("CIRCUS_DOCKER_PULL_POLICY") {
        Err(std::env::VarError::NotPresent) => ImagePullPolicy::LocalOnly,
        Ok(value) if value == "local-only" => ImagePullPolicy::LocalOnly,
        Ok(value) if value == "if-missing" => ImagePullPolicy::IfMissing,
        Ok(value) => return Err(format!("invalid CIRCUS_DOCKER_PULL_POLICY: {value}").into()),
        Err(error) => return Err(error.into()),
    };
    let image = prepare_docker_image(
        ImageRequest::new(
            std::env::var("CIRCUS_DOCKER_ENDPOINT")?,
            reference,
            platform,
            Duration::from_secs(120),
        )?
        .with_pull_policy(pull_policy),
    )
    .await?;
    println!("{} {}/{}", image.id(), image.os(), image.architecture());
    Ok(())
}
