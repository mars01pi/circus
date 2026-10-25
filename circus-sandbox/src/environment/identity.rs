//! Stable, opaque environment identity derived from resolved preparation input.
//!
//! The identity is a SHA-256 digest over a canonical, length-prefixed
//! serialization (format version 1) of:
//!
//! - the immutable image ID;
//! - the sorted set of repository digests reported by the local daemon;
//! - the requested platform: OS, Docker architecture, and optional variant;
//! - every ordered preparation step: exact argv and pinned version;
//! - the caller-supplied tenant security domain.
//!
//! Nothing else is included. Docker endpoint, host paths, environment
//! variables, credentials, command timeouts, output bounds, and observed
//! command output are never hashed. The value is a pure function of those
//! inputs: changing any included input produces a different identity, and
//! equivalent inputs (including a different order of the same repository
//! digests) produce the same identity.
//!
//! This module implements identity derivation only. There is no cache,
//! registry, persistence layer, or lookup service; the tenant domain is mixed
//! into the digest so identities from different security domains cannot
//! collide even for identical image and preparation input.

use std::fmt;

use super::ImagePlatform;
use super::plan::PrepareStep;
use super::sha256::Sha256;

const DOMAIN: &[u8] = b"circus-sandbox.environment-identity";
const FORMAT_VERSION: u64 = 1;

/// Caller-defined tenant security domain mixed into every [`EnvironmentId`].
///
/// The value is opaque to this crate and is not a credential or secret. It must
/// be nonempty and contain no whitespace or control characters so that two
/// differently spelled domains cannot name the same trust boundary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TenantDomain(String);

impl TenantDomain {
    /// Validates a caller-supplied tenant security domain.
    ///
    /// # Errors
    ///
    /// Returns [`IdentityError::MissingTenant`] for an empty value and
    /// [`IdentityError::InvalidTenant`] for whitespace or control characters.
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IdentityError::MissingTenant);
        }
        if value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err(IdentityError::InvalidTenant);
        }
        Ok(Self(value))
    }

    /// Exact validated domain bytes.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TenantDomain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Stable, opaque identity for one resolved environment.
///
/// Construction validates every included input and hashes it with SHA-256
/// (FIPS 180-4) using serialization format version 1. The digest is exposed as
/// 64 lowercase hexadecimal characters by [`Display`](fmt::Display); [`Debug`]
/// prints only that opaque value, never image, step, tenant, or host details.
///
/// Equality and ordering compare the digest bytes. Two values are equal exactly
/// when every included input was equivalent. The value carries no cache entry
/// or storage handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnvironmentId {
    digest: [u8; 32],
}

impl EnvironmentId {
    /// Derives an identity from resolved image contents, platform, ordered
    /// pinned steps, and a tenant security domain.
    ///
    /// Repository digests are sorted and compared as a set; their input order
    /// does not affect the result. Step order does affect the result. Each step
    /// must carry a pinned version and nonempty argv; an unpinned step is
    /// refused rather than silently accepted.
    ///
    /// # Errors
    ///
    /// Returns [`IdentityError`] for missing image ID, repository digest, step,
    /// or tenant values, malformed text, an unpinned step, or a duplicate
    /// repository digest.
    pub fn new(
        image_id: impl Into<String>,
        repo_digests: Vec<String>,
        platform: ImagePlatform,
        steps: Vec<PrepareStep>,
        tenant: TenantDomain,
    ) -> Result<Self, IdentityError> {
        let image_id = image_id.into();
        if image_id.is_empty() {
            return Err(IdentityError::MissingImageId);
        }
        if invalid_text(&image_id) {
            return Err(IdentityError::InvalidImageId);
        }

        let mut digests = Vec::with_capacity(repo_digests.len());
        for (index, digest) in repo_digests.into_iter().enumerate() {
            if digest.is_empty() {
                return Err(IdentityError::MissingRepoDigest { index });
            }
            if invalid_text(&digest) {
                return Err(IdentityError::InvalidRepoDigest { index });
            }
            if digests.iter().any(|existing| existing == &digest) {
                return Err(IdentityError::DuplicateRepoDigest { index });
            }
            digests.push(digest);
        }
        digests.sort_unstable();

        if steps.is_empty() {
            return Err(IdentityError::MissingSteps);
        }
        for (index, step) in steps.iter().enumerate() {
            if step.argv().is_empty() {
                return Err(IdentityError::EmptyStepArgv { index });
            }
            if step.version().is_none() {
                return Err(IdentityError::UnpinnedInput { index });
            }
        }

        let mut hasher = Sha256::new();
        hasher.update(DOMAIN);
        hasher.update(&FORMAT_VERSION.to_be_bytes());
        write_field(&mut hasher, image_id.as_bytes());
        hasher.update(&(digests.len() as u64).to_be_bytes());
        for digest in &digests {
            write_field(&mut hasher, digest.as_bytes());
        }
        write_field(&mut hasher, platform.os().as_bytes());
        write_field(&mut hasher, platform.architecture().as_bytes());
        match platform.variant() {
            Some(variant) => {
                hasher.update(&[1]);
                write_field(&mut hasher, variant.as_bytes());
            }
            None => hasher.update(&[0]),
        }
        hasher.update(&(steps.len() as u64).to_be_bytes());
        for (index, step) in steps.iter().enumerate() {
            hasher.update(&(step.argv().len() as u64).to_be_bytes());
            for argument in step.argv() {
                write_field(&mut hasher, argument.as_bytes());
            }
            let version = step
                .version()
                .ok_or(IdentityError::UnpinnedInput { index })?;
            write_field(&mut hasher, version.as_str().as_bytes());
        }
        write_field(&mut hasher, tenant.as_str().as_bytes());

        Ok(Self {
            digest: hasher.finish(),
        })
    }

    /// Raw SHA-256 digest bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.digest
    }
}

impl fmt::Display for EnvironmentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &byte in &self.digest {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Why environment identity material was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("image ID must be nonempty")]
    MissingImageId,
    #[error("image ID must not contain whitespace or control characters")]
    InvalidImageId,
    #[error("repository digest at index {index} is empty")]
    MissingRepoDigest { index: usize },
    #[error("repository digest at index {index} contains whitespace or control characters")]
    InvalidRepoDigest { index: usize },
    #[error("repository digest at index {index} duplicates an earlier digest")]
    DuplicateRepoDigest { index: usize },
    #[error("an environment identity requires at least one preparation step")]
    MissingSteps,
    #[error("preparation step {index} has no pinned version; reproducibility cannot be claimed")]
    UnpinnedInput { index: usize },
    #[error("preparation step {index} has empty argv")]
    EmptyStepArgv { index: usize },
    #[error("tenant security domain must be nonempty")]
    MissingTenant,
    #[error("tenant security domain must not contain whitespace or control characters")]
    InvalidTenant,
    #[error(
        "image platform {actual_os}/{actual_architecture} does not match requested {expected:?}"
    )]
    ImagePlatformMismatch {
        expected: ImagePlatform,
        actual_os: String,
        actual_architecture: String,
    },
}

fn invalid_text(value: &str) -> bool {
    value
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
}

fn write_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::environment::{PinnedVersion, PrepareStep, PreparedImage};

    fn platform() -> ImagePlatform {
        ImagePlatform::new("linux", "amd64").expect("valid platform")
    }

    fn step(program: &str, version: &str) -> PrepareStep {
        PrepareStep::new(
            vec![program.to_string(), "--version".to_string()],
            PinnedVersion::new(version).expect("valid version"),
            Duration::from_secs(5),
            4096,
        )
        .expect("valid step")
    }

    fn id(
        image: &str,
        digests: &[&str],
        platform: ImagePlatform,
        steps: Vec<PrepareStep>,
        tenant: &str,
    ) -> EnvironmentId {
        EnvironmentId::new(
            image.to_string(),
            digests.iter().map(|value| (*value).to_string()).collect(),
            platform,
            steps,
            TenantDomain::new(tenant).expect("valid tenant"),
        )
        .expect("valid identity")
    }

    #[test]
    fn equivalent_inputs_are_stable_and_digest_order_is_irrelevant() {
        let first = id(
            "sha256:1111",
            &["repo@sha256:aaa", "repo@sha256:bbb"],
            platform(),
            vec![step("python3", "3.12.1")],
            "tenant-a",
        );
        let second = id(
            "sha256:1111",
            &["repo@sha256:bbb", "repo@sha256:aaa"],
            platform(),
            vec![step("python3", "3.12.1")],
            "tenant-a",
        );
        assert_eq!(first, second);
        assert_eq!(first.to_string().len(), 64);
        assert!(
            first
                .to_string()
                .chars()
                .all(|value| value.is_ascii_hexdigit())
        );
    }

    #[test]
    fn fixed_input_matches_documented_format_v1() {
        let value = id(
            "sha256:1111",
            &["repo@sha256:bbb", "repo@sha256:aaa"],
            platform(),
            vec![step("python3", "3.12.1")],
            "tenant-a",
        );
        assert_eq!(
            value.to_string(),
            "65bb858bc92cb741ced70a326a3e2a54dece80ea6d43325055e5c60f9b6075c5"
        );
    }

    #[test]
    fn every_included_input_changes_the_identity() {
        let base = id(
            "sha256:1111",
            &["repo@sha256:aaa"],
            platform(),
            vec![step("python3", "3.12.1")],
            "tenant-a",
        );
        let different_image = id(
            "sha256:2222",
            &["repo@sha256:aaa"],
            platform(),
            vec![step("python3", "3.12.1")],
            "tenant-a",
        );
        let different_digest = id(
            "sha256:1111",
            &["repo@sha256:ccc"],
            platform(),
            vec![step("python3", "3.12.1")],
            "tenant-a",
        );
        let different_step = id(
            "sha256:1111",
            &["repo@sha256:aaa"],
            platform(),
            vec![step("python3", "3.12.2")],
            "tenant-a",
        );
        let different_tenant = id(
            "sha256:1111",
            &["repo@sha256:aaa"],
            platform(),
            vec![step("python3", "3.12.1")],
            "tenant-b",
        );
        let different_variant = id(
            "sha256:1111",
            &["repo@sha256:aaa"],
            ImagePlatform::with_variant("linux", "amd64", Some("v3".to_string()))
                .expect("valid variant"),
            vec![step("python3", "3.12.1")],
            "tenant-a",
        );
        for changed in [
            different_image,
            different_digest,
            different_step,
            different_tenant,
            different_variant,
        ] {
            assert_ne!(base, changed);
        }
    }

    #[test]
    fn display_and_debug_are_opaque() {
        let value = id(
            "sha256:1111",
            &["repo@sha256:aaa"],
            platform(),
            vec![step("/usr/local/bin/python3", "3.12.1-secret")],
            "tenant-secret-domain",
        );
        let display = value.to_string();
        let debug = format!("{value:?}");
        assert_eq!(display.len(), 64);
        for secret in [
            "tenant-secret-domain",
            "/usr/local/bin/python3",
            "3.12.1-secret",
            "sha256:1111",
            "repo@sha256:aaa",
        ] {
            assert!(!display.contains(secret));
            assert!(!debug.contains(secret));
        }
    }

    #[test]
    fn rejects_missing_excess_and_unpinned_input() {
        let tenant = TenantDomain::new("tenant-a").expect("valid tenant");
        assert_eq!(
            EnvironmentId::new(
                "",
                Vec::new(),
                platform(),
                vec![step("python3", "3.12.1")],
                tenant.clone()
            ),
            Err(IdentityError::MissingImageId)
        );
        assert_eq!(
            EnvironmentId::new(
                "sha256:1111",
                vec!["repo@sha256:aaa".into(), "repo@sha256:aaa".into()],
                platform(),
                vec![step("python3", "3.12.1")],
                tenant.clone()
            ),
            Err(IdentityError::DuplicateRepoDigest { index: 1 })
        );
        assert_eq!(
            EnvironmentId::new(
                "sha256:1111",
                Vec::new(),
                platform(),
                Vec::new(),
                tenant.clone()
            ),
            Err(IdentityError::MissingSteps)
        );
        let unpinned = PrepareStep::unpinned(
            vec!["python3".into(), "--version".into()],
            Duration::from_secs(1),
            1024,
        )
        .expect("valid argv");
        assert_eq!(
            EnvironmentId::new(
                "sha256:1111",
                Vec::new(),
                platform(),
                vec![unpinned],
                tenant
            ),
            Err(IdentityError::UnpinnedInput { index: 0 })
        );
        assert_eq!(
            TenantDomain::new("tenant a"),
            Err(IdentityError::InvalidTenant)
        );
        assert_eq!(TenantDomain::new(""), Err(IdentityError::MissingTenant));
    }

    #[test]
    fn platform_check_uses_actual_image_metadata() {
        let image = PreparedImage {
            id: "sha256:1111".into(),
            repo_digests: Vec::new(),
            os: "linux".into(),
            architecture: "arm64".into(),
        };
        assert!(matches!(
            super::super::plan::verify_image_platform(&image, &platform()),
            Err(IdentityError::ImagePlatformMismatch { .. })
        ));
    }
}
