use std::time::Duration;

use circus_sandbox::environment::{
    EnvironmentError, ImagePlatform, ImagePullPolicy, ImageRequest, prepare_docker_image,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn request(reference: &str) -> TestResult<ImageRequest> {
    assert_eq!(std::env::var("CIRCUS_DOCKER_SANDBOX").as_deref(), Ok("1"));
    assert_eq!(std::env::var("CIRCUS_DOCKER_PULL").as_deref(), Ok("1"));
    Ok(ImageRequest::new(
        std::env::var("CIRCUS_DOCKER_ENDPOINT")?,
        reference,
        ImagePlatform::new("linux", std::env::var("CIRCUS_DOCKER_ARCH")?)?,
        Duration::from_secs(120),
    )?)
}

#[tokio::test]
#[ignore = "requires explicit network pull opt-in and initially absent public CIRCUS_DOCKER_PULL_IMAGE"]
async fn missing_public_image_is_pulled_and_resolves_by_immutable_id() -> TestResult {
    let reference = std::env::var("CIRCUS_DOCKER_PULL_IMAGE")?;
    let local = prepare_docker_image(request(&reference)?).await;
    assert!(
        matches!(local, Err(EnvironmentError::ImageNotFound { reference: found }) if found == reference)
    );
    let image =
        prepare_docker_image(request(&reference)?.with_pull_policy(ImagePullPolicy::IfMissing))
            .await?;
    assert!(image.id().starts_with("sha256:"));
    assert_eq!(image.id().len(), 71);
    assert!(image.id()[7..].bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(image.os(), "linux");
    assert_eq!(image.architecture(), std::env::var("CIRCUS_DOCKER_ARCH")?);
    let cached =
        prepare_docker_image(request(image.id())?.with_pull_policy(ImagePullPolicy::IfMissing))
            .await?;
    assert_eq!(image.id(), cached.id());
    assert_eq!(image.repo_digests(), cached.repo_digests());
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit real Docker Engine and network pull opt-in"]
async fn absent_registry_tag_reports_pull_failure() -> TestResult {
    let reference = format!(
        "docker.io/library/python:circus-missing-{}",
        uuid::Uuid::new_v4()
    );
    let local = prepare_docker_image(request(&reference)?).await;
    assert!(
        matches!(local, Err(EnvironmentError::ImageNotFound { reference: found }) if found == reference)
    );
    let result =
        prepare_docker_image(request(&reference)?.with_pull_policy(ImagePullPolicy::IfMissing))
            .await;
    assert!(matches!(result, Err(EnvironmentError::Engine { .. })));
    Ok(())
}
