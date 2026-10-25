//! Explicit containerd acquisition and immutable manifest-name registration.
//!
//! Transfer object fields follow containerd-client's container_pull example and
//! api/types/transfer/{registry,imagestore}.proto. Preparation never unpacks;
//! sandbox creation retains ownership of the existing ensure_rootfs path.

use containerd_client::services::v1::{
    CreateImageRequest, GetImageRequest, Image, TransferRequest, images_client::ImagesClient,
    transfer_client::TransferClient,
};
use containerd_client::tonic::{Code, Request, Status, transport::Channel};
use containerd_client::types::{
    Descriptor, Platform,
    transfer::{ImageStore, OciRegistry},
};
use containerd_client::{to_any, with_namespace};

use super::containerd_image::{
    INDEX_MEDIA_TYPES, descriptor_from_value, get_image, image_candidates, parse_manifest,
    read_content,
};
use super::engine::EngineError;

const MANIFEST_MEDIA_TYPES: [&str; 2] = [
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];

/// Borrowed, already validated adapter inputs; no public domain or SDK types.
pub(crate) struct ContainerdImageSpec<'a> {
    pub socket: &'a str,
    pub namespace: &'a str,
    pub reference: &'a str,
    pub platform: ContainerdPlatform<'a>,
    pub pull_if_missing: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct ContainerdPlatform<'a> {
    pub os: &'a str,
    pub architecture: &'a str,
    pub variant: Option<&'a str>,
}

impl ContainerdImageSpec<'_> {
    fn socket(&self) -> &str {
        self.socket
    }
    fn namespace(&self) -> &str {
        self.namespace
    }
    fn reference(&self) -> &str {
        self.reference
    }
    fn platform(&self) -> &ContainerdPlatform<'_> {
        &self.platform
    }
}

impl ContainerdPlatform<'_> {
    fn os(&self) -> &str {
        self.os
    }
    fn architecture(&self) -> &str {
        self.architecture
    }
    fn variant(&self) -> Option<&str> {
        self.variant
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ContainerdPlatformInfo {
    pub os: String,
    pub architecture: String,
    pub variant: Option<String>,
}

impl ContainerdPlatformInfo {
    fn inspected(os: &str, architecture: &str, variant: Option<&str>) -> Option<Self> {
        if variant
            .is_some_and(|variant| variant.chars().any(|c| c.is_whitespace() || c.is_control()))
        {
            return None;
        }
        Some(Self {
            os: os.to_string(),
            architecture: architecture.to_string(),
            variant: variant.map(str::to_string),
        })
    }
    fn os(&self) -> &str {
        &self.os
    }
    fn architecture(&self) -> &str {
        &self.architecture
    }
    fn variant(&self) -> Option<&str> {
        self.variant.as_deref()
    }
}

pub(crate) struct ContainerdPreparedImage {
    pub reference: String,
    pub digest: String,
    pub platform: ContainerdPlatformInfo,
}

#[derive(Debug)]
pub(crate) enum ContainerdImageFailure {
    ImageNotFound,
    PlatformMismatch {
        actual: Option<ContainerdPlatformInfo>,
    },
    AliasConflict {
        reference: String,
    },
    Daemon {
        message: String,
    },
}

pub(crate) async fn prepare_containerd_image(
    request: &ContainerdImageSpec<'_>,
) -> Result<ContainerdPreparedImage, ContainerdImageFailure> {
    let channel = containerd_client::connect(request.socket())
        .await
        .map_err(|_| daemon("connecting to containerd failed"))?;
    let pull_name = canonical_pull_name(request.reference());
    let image = match lookup(&channel, request, &pull_name).await {
        Ok(image) => image,
        Err(error) if error.not_found && request.pull_if_missing => {
            pull(&channel, request, &pull_name).await?;
            get_image(&channel, request.namespace(), &pull_name)
                .await
                .map_err(lookup_error)?
        }
        Err(error) => return Err(lookup_error(error)),
    };
    let target = image
        .target
        .ok_or_else(|| daemon("image record has no target"))?;
    let direct_manifest = MANIFEST_MEDIA_TYPES.contains(&target.media_type.as_str());
    let (manifest, index_variant) = select_manifest(&channel, request, target).await?;
    let bytes = read_metadata(&channel, request.namespace(), &manifest).await?;
    let parsed = parse_manifest(&bytes).map_err(|_| daemon("invalid image manifest metadata"))?;
    let config = read_metadata(&channel, request.namespace(), &parsed.config).await?;
    let platform = inspect_platform(&config, index_variant.as_deref(), request.platform())?;
    let reference = immutable_reference(&image.name, &manifest.digest)?;
    // The direct-manifest selection returns the original descriptor unchanged.
    // An already loaded exact immutable record needs no Create/AlreadyExists/Get.
    if !direct_manifest || image.name != reference {
        register_alias(&channel, request.namespace(), &reference, &manifest).await?;
    }
    Ok(ContainerdPreparedImage {
        reference,
        digest: manifest.digest,
        platform,
    })
}

async fn lookup(
    channel: &Channel,
    request: &ContainerdImageSpec<'_>,
    pull_name: &str,
) -> Result<Image, EngineError> {
    match get_image(channel, request.namespace(), request.reference()).await {
        Err(error) if error.not_found && needs_default_tag(request.reference()) => {
            get_image(channel, request.namespace(), pull_name).await
        }
        result => result,
    }
}

fn canonical_pull_name(reference: &str) -> String {
    let normalized = image_candidates(reference).pop().unwrap_or_default();
    if !needs_default_tag(&normalized) {
        normalized
    } else {
        format!("{normalized}:latest")
    }
}

fn needs_default_tag(reference: &str) -> bool {
    !reference.contains('@')
        && !reference
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .contains(':')
}

async fn pull(
    channel: &Channel,
    request: &ContainerdImageSpec<'_>,
    reference: &str,
) -> Result<(), ContainerdImageFailure> {
    // No auth callback, headers, host_dir, or client credential sources. No
    // unpack configuration: prepare only the content/image metadata cache.
    let source = OciRegistry {
        reference: reference.to_string(),
        resolver: None,
    };
    let destination = ImageStore {
        name: reference.to_string(),
        platforms: vec![Platform {
            os: request.platform().os().to_string(),
            architecture: request.platform().architecture().to_string(),
            variant: request.platform().variant().unwrap_or_default().to_string(),
            os_version: String::new(),
        }],
        ..Default::default()
    };
    let transfer = TransferRequest {
        source: Some(to_any(&source)),
        destination: Some(to_any(&destination)),
        options: None,
    };
    TransferClient::new(channel.clone())
        .transfer(with_namespace!(transfer, request.namespace()))
        .await
        .map_err(|status| status_error("registry transfer", status))?;
    Ok(())
}

async fn select_manifest(
    channel: &Channel,
    request: &ContainerdImageSpec<'_>,
    target: Descriptor,
) -> Result<(Descriptor, Option<String>), ContainerdImageFailure> {
    if !INDEX_MEDIA_TYPES.contains(&target.media_type.as_str()) {
        validate_manifest_descriptor(&target)?;
        return Ok((target, None));
    }
    let bytes = read_metadata(channel, request.namespace(), &target).await?;
    let index: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| daemon("invalid image index JSON"))?;
    let entries = index
        .get("manifests")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| daemon("image index has no manifests"))?;
    let selected = entries
        .iter()
        .find(|entry| {
            let platform = entry.get("platform");
            let field = |name: &str| {
                platform
                    .and_then(|value| value.get(name))
                    .and_then(serde_json::Value::as_str)
            };
            field("os") == Some(request.platform().os())
                && field("architecture") == Some(request.platform().architecture())
                && request.platform().variant().is_none_or(|variant| {
                    field("variant").filter(|value| !value.is_empty()) == Some(variant)
                })
        })
        .ok_or(ContainerdImageFailure::PlatformMismatch { actual: None })?;
    let manifest =
        descriptor_from_value(selected).map_err(|_| daemon("invalid image index descriptor"))?;
    validate_manifest_descriptor(&manifest)?;
    let variant = selected
        .get("platform")
        .and_then(|platform| platform.get("variant"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Ok((manifest, variant))
}

fn validate_manifest_descriptor(descriptor: &Descriptor) -> Result<(), ContainerdImageFailure> {
    if !MANIFEST_MEDIA_TYPES.contains(&descriptor.media_type.as_str()) {
        return Err(daemon("selected image target is not a supported manifest"));
    }
    validate_digest(&descriptor.digest)
}

async fn read_metadata(
    channel: &Channel,
    namespace: &str,
    descriptor: &Descriptor,
) -> Result<Vec<u8>, ContainerdImageFailure> {
    validate_digest(&descriptor.digest)?;
    if descriptor.size < 0 {
        return Err(daemon("image metadata descriptor has a negative size"));
    }
    read_content(channel, namespace, descriptor)
        .await
        .map_err(|_| daemon("reading bounded image metadata failed"))
}

fn inspect_platform(
    bytes: &[u8],
    index_variant: Option<&str>,
    expected: &ContainerdPlatform<'_>,
) -> Result<ContainerdPlatformInfo, ContainerdImageFailure> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| daemon("invalid image config JSON"))?;
    let os = value
        .get("os")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| daemon("image config has no OS"))?;
    let architecture = value
        .get("architecture")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| daemon("image config has no architecture"))?;
    let config_variant = match value.get("variant") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| daemon("image config variant is not a string"))?,
        )
        .filter(|value| !value.is_empty()),
    };
    // Index variant evidence belongs only to the index's selected OS/arch.
    // Report a contradictory config unchanged rather than grafting that
    // evidence onto a different platform.
    if os != expected.os() || architecture != expected.architecture() {
        return Err(ContainerdImageFailure::PlatformMismatch {
            actual: ContainerdPlatformInfo::inspected(os, architecture, config_variant),
        });
    }
    let conflict = config_variant
        .zip(index_variant)
        .is_some_and(|(config, index)| config != index);
    // A missing config variant can use the selected index's explicit evidence;
    // neither source is replaced with the host platform or a guessed default.
    let actual =
        ContainerdPlatformInfo::inspected(os, architecture, config_variant.or(index_variant));
    let matches = actual.as_ref().is_some_and(|actual| {
        actual.os() == expected.os()
            && actual.architecture() == expected.architecture()
            && expected
                .variant()
                .is_none_or(|variant| actual.variant() == Some(variant))
    });
    if conflict || !matches {
        return Err(ContainerdImageFailure::PlatformMismatch { actual });
    }
    actual.ok_or_else(|| daemon("image config has no supported platform"))
}

fn immutable_reference(name: &str, digest: &str) -> Result<String, ContainerdImageFailure> {
    validate_digest(digest)?;
    let repository = name.split('@').next().unwrap_or_default();
    let slash = repository.rfind('/');
    let repository = match repository.rfind(':') {
        Some(colon) if slash.is_none_or(|slash| colon > slash) => &repository[..colon],
        _ => repository,
    };
    if repository.is_empty()
        || repository.ends_with('/')
        || repository.contains("://")
        || repository
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(daemon("image record has no usable repository name"));
    }
    Ok(format!("{repository}@{digest}"))
}

fn validate_digest(digest: &str) -> Result<(), ContainerdImageFailure> {
    let valid = digest.split_once(':').is_some_and(|(algorithm, encoded)| {
        let expected_length = match algorithm {
            "sha256" => 64,
            "sha512" => 128,
            _ => return false,
        };
        encoded.len() == expected_length && encoded.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    if valid {
        Ok(())
    } else {
        Err(daemon(
            "image descriptor has an unsupported or invalid digest",
        ))
    }
}

async fn register_alias(
    channel: &Channel,
    namespace: &str,
    reference: &str,
    target: &Descriptor,
) -> Result<(), ContainerdImageFailure> {
    let mut client = ImagesClient::new(channel.clone());
    let create = CreateImageRequest {
        image: Some(Image {
            name: reference.to_string(),
            target: Some(target.clone()),
            ..Default::default()
        }),
        source_date_epoch: None,
    };
    match client.create(with_namespace!(create, namespace)).await {
        Ok(response) => {
            let image = response
                .into_inner()
                .image
                .ok_or_else(|| daemon("created image alias response has no image"))?;
            if image.name != reference || !same_target(image.target.as_ref(), target) {
                return Err(ContainerdImageFailure::AliasConflict {
                    reference: reference.to_string(),
                });
            }
            Ok(())
        }
        Err(status) if status.code() == Code::AlreadyExists => {
            let image = client
                .get(with_namespace!(
                    GetImageRequest {
                        name: reference.to_string()
                    },
                    namespace
                ))
                .await
                .map_err(|status| status_error("reading existing immutable alias", status))?
                .into_inner()
                .image
                .ok_or_else(|| daemon("existing image alias response has no image"))?;
            if image.name == reference && same_target(image.target.as_ref(), target) {
                Ok(())
            } else {
                Err(ContainerdImageFailure::AliasConflict {
                    reference: reference.to_string(),
                })
            }
        }
        Err(status) => Err(status_error("registering immutable image alias", status)),
    }
}

fn same_target(actual: Option<&Descriptor>, expected: &Descriptor) -> bool {
    actual.is_some_and(|actual| {
        actual.digest == expected.digest
            && actual.media_type == expected.media_type
            && actual.size == expected.size
    })
}

fn lookup_error(error: EngineError) -> ContainerdImageFailure {
    if error.not_found {
        ContainerdImageFailure::ImageNotFound
    } else {
        daemon("containerd image lookup failed")
    }
}

// Never surface daemon message text: registry errors may contain URLs, bearer
// challenges, or credentials. A static phase and status code retain safe context.
fn status_error(phase: &str, status: Status) -> ContainerdImageFailure {
    daemon(&format!("{phase} failed ({:?})", status.code()))
}

fn daemon(message: &str) -> ContainerdImageFailure {
    ContainerdImageFailure::Daemon {
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_strip_tags_but_preserve_registry_ports_and_digest_repositories() {
        let digest = format!("sha256:{}", "a".repeat(64));
        for (record, repository) in [
            ("localhost:5000/team/image:tag", "localhost:5000/team/image"),
            ("localhost:5000/team/image", "localhost:5000/team/image"),
            (
                "docker.io/library/python:3.12-slim",
                "docker.io/library/python",
            ),
            ("team/image@sha256:old", "team/image"),
            ("image:tag", "image"),
        ] {
            assert_eq!(
                immutable_reference(record, &digest).unwrap(),
                format!("{repository}@{digest}")
            );
        }
        assert!(immutable_reference("", &digest).is_err());
        assert!(immutable_reference("image", "sha256:short").is_err());
    }

    #[test]
    fn pull_names_have_one_default_tag_and_keep_ports_and_digests() {
        assert_eq!(
            canonical_pull_name("python"),
            "docker.io/library/python:latest"
        );
        assert_eq!(
            canonical_pull_name("python:3.12-slim"),
            "docker.io/library/python:3.12-slim"
        );
        assert_eq!(
            canonical_pull_name("localhost:5000/team/image"),
            "localhost:5000/team/image:latest"
        );
        assert_eq!(
            canonical_pull_name("team/image@sha256:abc"),
            "docker.io/team/image@sha256:abc"
        );
    }

    #[test]
    fn config_platform_is_inspected_and_not_inferred_from_host() {
        let expected = ContainerdPlatform {
            os: "linux",
            architecture: "arm",
            variant: None,
        };
        let actual = inspect_platform(
            br#"{"os":"linux","architecture":"arm","variant":"v7"}"#,
            None,
            &expected,
        )
        .unwrap();
        assert_eq!(
            actual,
            ContainerdPlatformInfo::inspected("linux", "arm", Some("v7")).unwrap()
        );
        assert!(matches!(
            inspect_platform(
                br#"{"os":"windows","architecture":"amd64"}"#,
                None,
                &expected
            ),
            Err(ContainerdImageFailure::PlatformMismatch { .. })
        ));
        assert!(inspect_platform(br#"{"os":"linux"}"#, None, &expected).is_err());
        assert!(
            inspect_platform(
                br#"{"os":"linux","architecture":"amd64","variant":5}"#,
                None,
                &expected,
            )
            .is_err()
        );
    }
}
