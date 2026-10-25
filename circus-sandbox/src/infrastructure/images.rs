//! Image inspection and explicit unauthenticated pulls through the selected Engine client.

use bollard::models::CreateImageInfo;
use bollard::query_parameters::{CreateImageOptions, CreateImageOptionsBuilder};
use futures_util::StreamExt;

use super::{Engine, EngineError, map_err};

pub(crate) struct ImageInfo {
    pub id: String,
    pub repo_digests: Vec<String>,
    pub os: String,
    pub architecture: String,
}

impl Engine {
    /// Inspects one local image. Never contacts a registry or pulls.
    pub(crate) async fn inspect_image(&self, reference: &str) -> Result<ImageInfo, EngineError> {
        let image = self
            .docker
            .inspect_image(reference)
            .await
            .map_err(map_err)?;
        Ok(ImageInfo {
            id: required(image.id, "id")?,
            // Locally built/imported images need not have any registry digest.
            repo_digests: image.repo_digests.unwrap_or_default(),
            os: required(image.os, "os")?,
            architecture: required(image.architecture, "architecture")?,
        })
    }

    /// Pulls one explicitly selected image/platform. The caller owns the deadline.
    pub(crate) async fn pull_image(
        &self,
        reference: &str,
        platform: &str,
    ) -> Result<(), EngineError> {
        // None explicitly supplies no registry credentials. Do not consult the
        // Docker CLI configuration, context, or environment for authentication.
        let mut progress =
            self.docker
                .create_image(Some(pull_options(reference, platform)), None, None);
        while let Some(item) = progress.next().await {
            check_progress(item.map_err(map_err)?)?;
        }
        Ok(())
    }
}

fn pull_options(reference: &str, platform: &str) -> CreateImageOptions {
    let options = CreateImageOptionsBuilder::new()
        .from_image(reference)
        .platform(platform);
    // Docker accepts a full tag/digest in fromImage. A separate tag would
    // override an embedded tag; an unqualified repository without tag=latest
    // would instead pull every tag. Registry ports are not image tags.
    let has_selector = reference.contains('@')
        || reference
            .rsplit('/')
            .next()
            .is_some_and(|name| name.contains(':'));
    if has_selector {
        options.build()
    } else {
        options.tag("latest").build()
    }
}

fn check_progress(info: CreateImageInfo) -> Result<(), EngineError> {
    // Bollard turns errorDetail.message into a stream error, but leaves
    // message-less errorDetail objects as successful items. Their presence is
    // still a failure, even with no message or only a code.
    if info.error_detail.is_some() {
        return Err(EngineError::new(
            "Docker image pull reported an error without a valid message",
        ));
    }
    Ok(())
}

fn required(value: Option<String>, field: &str) -> Result<String, EngineError> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| EngineError::new(format!("image inspection omitted required {field}")))
}
