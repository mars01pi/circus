//! Kubernetes workload descriptor validation. No SDK type is involved, so the
//! descriptor is validated even when the `kubernetes` feature is absent.

use std::path::Path;

use super::CommonSpec;

/// Validated Kubernetes-only create configuration. Kept out of the public API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KubernetesConfig {
    kubeconfig: String,
    context: Option<String>,
    namespace: String,
    image: String,
    service_account: Option<String>,
}

impl KubernetesConfig {
    /// Validates the kubeconfig path, namespace, image reference, and optional
    /// context/service account. The kubeconfig is not read here.
    pub(crate) fn new(
        kubeconfig: &str,
        context: Option<&str>,
        namespace: &str,
        image: &str,
        service_account: Option<&str>,
    ) -> Result<Self, KubernetesConfigError> {
        if kubeconfig.is_empty() || !absolute_path(kubeconfig) {
            return Err(KubernetesConfigError::InvalidKubeconfigPath {
                value: kubeconfig.to_string(),
            });
        }
        if !dns_label(namespace) {
            return Err(KubernetesConfigError::InvalidNamespace {
                value: namespace.to_string(),
            });
        }
        if image.is_empty() {
            return Err(KubernetesConfigError::ImageRequired);
        }
        if !image_reference(image) {
            return Err(KubernetesConfigError::InvalidImageReference {
                value: image.to_string(),
            });
        }
        if let Some(context) = context
            && !context_name(context)
        {
            return Err(KubernetesConfigError::InvalidContextName {
                value: context.to_string(),
            });
        }
        if let Some(account) = service_account
            && !dns_subdomain(account)
        {
            return Err(KubernetesConfigError::InvalidServiceAccount {
                value: account.to_string(),
            });
        }
        Ok(Self {
            kubeconfig: kubeconfig.to_string(),
            context: context.map(str::to_string),
            namespace: namespace.to_string(),
            image: image.to_string(),
            service_account: service_account.map(str::to_string),
        })
    }

    /// Kubernetes force-runs the container as the guest uid and requires
    /// `runAsNonRoot`, so uid 0 is rejected before allocation.
    pub(crate) fn validate_common(&self, spec: &CommonSpec) -> Result<(), KubernetesConfigError> {
        if spec.guest_uid == 0 {
            return Err(KubernetesConfigError::RootUserUnsupported { uid: 0 });
        }
        Ok(())
    }

    pub(crate) fn kubeconfig(&self) -> &str {
        &self.kubeconfig
    }

    pub(crate) fn context(&self) -> Option<&str> {
        self.context.as_deref()
    }

    pub(crate) fn namespace(&self) -> &str {
        &self.namespace
    }

    pub(crate) fn image(&self) -> &str {
        &self.image
    }

    pub(crate) fn service_account(&self) -> Option<&str> {
        self.service_account.as_deref()
    }
}

/// Why Kubernetes configuration was rejected before any Pod allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KubernetesConfigError {
    InvalidKubeconfigPath { value: String },
    InvalidNamespace { value: String },
    InvalidContextName { value: String },
    InvalidServiceAccount { value: String },
    InvalidImageReference { value: String },
    ImageRequired,
    RootUserUnsupported { uid: u32 },
}

/// A DNS-1123 label: lowercase alphanumerics and interior hyphens, at most 63.
fn dns_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value.starts_with(|ch: char| ch.is_ascii_alphanumeric())
        && value.ends_with(|ch: char| ch.is_ascii_alphanumeric())
}

/// A DNS-1123 subdomain: dot-separated lowercase labels, at most 253 bytes.
fn dns_subdomain(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value.split('.').all(dns_label)
        && !value.ends_with('.')
}

/// Accepts a host-absolute path and also Unix absolute paths when the code is
/// built on Windows, because Kubernetes credentials commonly use `/...` paths.
fn absolute_path(value: &str) -> bool {
    value.starts_with('/') || Path::new(value).is_absolute()
}

fn context_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && !value.chars().any(|ch| ch.is_control() || ch == '\0')
        && value.trim() == value
}

/// Conservative Docker/OCI reference validation.
///
/// Kubelet rejects malformed references only after scheduling, so the adapter
/// rejects them before allocation. Repositories must be lowercase; registry
/// hosts and tags follow the distribution grammar closely enough to avoid
/// accepting or rejecting ordinary references inadvertently.
fn image_reference(value: &str) -> bool {
    if value.len() > 512
        || value
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return false;
    }
    let (name, _has_digest) = match value.split_once('@') {
        Some((name, digest)) => {
            let (algorithm, encoded) = match digest.split_once(':') {
                Some(parts) => parts,
                None => return false,
            };
            if algorithm.is_empty()
                || encoded.len() < 32
                || !encoded
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() || byte == b'_' || byte == b'-')
            {
                return false;
            }
            (name, true)
        }
        None => (value, false),
    };
    let (name, tag) = match name.rsplit_once(':') {
        Some((name, tag)) if !tag.contains('/') => (name, Some(tag)),
        _ => (name, None),
    };
    if let Some(tag) = tag
        && (tag.is_empty()
            || tag.len() > 128
            || !tag
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')))
    {
        return false;
    }
    if name.is_empty() || name.len() > 255 || name.starts_with('/') || name.ends_with('/') {
        return false;
    }
    let mut components = name.split('/');
    let first = components.next().unwrap_or_default();
    // A first component containing '.' or ':' is a registry, which may carry a
    // port; it still must not be empty or contain repository punctuation twice.
    let repository = if name.contains('/') && (first.contains('.') || first.contains(':')) {
        if !registry(first) {
            return false;
        }
        components.collect::<Vec<_>>()
    } else {
        name.split('/').collect()
    };
    if repository.is_empty() {
        return false;
    }
    for component in repository {
        if component.is_empty()
            || !component.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
            || component.starts_with(|ch: char| !ch.is_ascii_alphanumeric())
            || component.ends_with(|ch: char| !ch.is_ascii_alphanumeric())
        {
            return false;
        }
    }
    true
}

fn registry(value: &str) -> bool {
    let (host, port) = match value.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (value, None),
    };
    if let Some(port) = port
        && (port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return false;
    }
    !host.is_empty()
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_references() {
        for image in [
            "python:3.12-slim",
            "docker.io/library/python:3.12-slim",
            "localhost:5000/circus/python@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "ghcr.io/org/image",
        ] {
            assert!(image_reference(image), "{image}");
        }
    }

    #[test]
    fn rejects_malformed_references() {
        for image in [
            "",
            "Upper/Case:tag",
            "image:",
            "image:bad tag",
            "/absolute",
            "trailing/",
            "double//slash",
            "registry:abc/repo",
        ] {
            assert!(!image_reference(image), "{image}");
        }
    }

    #[test]
    fn validates_identity_fields() {
        assert!(dns_label("sandboxes"));
        assert!(!dns_label("Sandboxes"));
        assert!(!dns_label("-bad"));
        assert!(!dns_label("bad-"));
        assert!(dns_subdomain("sandbox.runner"));
        assert!(!dns_subdomain("sandbox..runner"));
        assert!(context_name("circus"));
        assert!(!context_name(" "));
        assert!(
            KubernetesConfig::new(
                "/etc/rancher/k3s/k3s.yaml",
                None,
                "default",
                "python:3.12-slim",
                None,
            )
            .is_ok()
        );
        assert!(matches!(
            KubernetesConfig::new("relative.yaml", None, "default", "python:3.12-slim", None),
            Err(KubernetesConfigError::InvalidKubeconfigPath { .. })
        ));
        assert!(matches!(
            KubernetesConfig::new(
                "/etc/rancher/k3s/k3s.yaml",
                None,
                "Default",
                "python:3.12-slim",
                None
            ),
            Err(KubernetesConfigError::InvalidNamespace { .. })
        ));
    }
}
