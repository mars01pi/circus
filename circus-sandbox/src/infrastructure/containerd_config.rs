//! containerd descriptor validation. Plain data; no SDK type crosses this
//! module and no connection is made here.

/// Validated containerd configuration. Kept out of the public API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerdConfig {
    socket: String,
    namespace: String,
    image: String,
    snapshotter: String,
}

impl ContainerdConfig {
    /// Validates the socket, namespace, image, runtime, and snapshotter before
    /// any instance allocation. `runsc` is rejected as unavailable on this
    /// host; only `runc` through containerd's runc v2 handler is implemented.
    pub(crate) fn new(
        socket: &str,
        namespace: &str,
        image: &str,
        runtime: &str,
        snapshotter: &str,
    ) -> Result<Self, ContainerdConfigError> {
        let socket = socket.trim();
        if !socket.starts_with('/') || socket.contains('\0') || socket.contains(' ') {
            return Err(ContainerdConfigError::InvalidSocketPath {
                value: socket.to_string(),
            });
        }
        if namespace.is_empty()
            || !namespace
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err(ContainerdConfigError::InvalidNamespace {
                value: namespace.to_string(),
            });
        }
        if image.is_empty() || image.chars().any(char::is_whitespace) {
            return Err(ContainerdConfigError::ImageRequired);
        }
        if runtime != "runc" {
            return Err(ContainerdConfigError::RuntimeUnavailable {
                name: runtime.to_string(),
            });
        }
        if snapshotter != "overlayfs" {
            return Err(ContainerdConfigError::UnsupportedSnapshotter {
                name: snapshotter.to_string(),
            });
        }
        Ok(Self {
            socket: socket.to_string(),
            namespace: namespace.to_string(),
            image: image.to_string(),
            snapshotter: snapshotter.to_string(),
        })
    }

    #[cfg(unix)]
    pub(crate) fn socket(&self) -> &str {
        &self.socket
    }

    #[cfg(unix)]
    pub(crate) fn namespace(&self) -> &str {
        &self.namespace
    }

    #[cfg(unix)]
    pub(crate) fn image(&self) -> &str {
        &self.image
    }

    #[cfg(unix)]
    pub(crate) fn snapshotter(&self) -> &str {
        &self.snapshotter
    }
}

/// Why containerd configuration was rejected before any instance allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContainerdConfigError {
    InvalidSocketPath { value: String },
    InvalidNamespace { value: String },
    ImageRequired,
    RuntimeUnavailable { name: String },
    UnsupportedSnapshotter { name: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOCKET: &str = "/run/containerd/containerd.sock";

    #[test]
    fn rejects_invalid_socket_and_namespace() {
        for socket in ["", "unix:///run/containerd/containerd.sock", "run.sock"] {
            assert!(matches!(
                ContainerdConfig::new(socket, "default", "image", "runc", "overlayfs"),
                Err(ContainerdConfigError::InvalidSocketPath { .. })
            ));
        }
        for namespace in ["", "has space", "a/b", "名前"] {
            assert!(matches!(
                ContainerdConfig::new(SOCKET, namespace, "image", "runc", "overlayfs"),
                Err(ContainerdConfigError::InvalidNamespace { .. })
            ));
        }
    }

    #[test]
    fn rejects_runsc_and_unknown_snapshotters_explicitly() {
        assert!(matches!(
            ContainerdConfig::new(SOCKET, "default", "image", "runsc", "overlayfs"),
            Err(ContainerdConfigError::RuntimeUnavailable { .. })
        ));
        assert!(matches!(
            ContainerdConfig::new(SOCKET, "default", "image", "kata", "overlayfs"),
            Err(ContainerdConfigError::RuntimeUnavailable { .. })
        ));
        assert!(matches!(
            ContainerdConfig::new(SOCKET, "default", "image", "runc", "native"),
            Err(ContainerdConfigError::UnsupportedSnapshotter { .. })
        ));
        assert!(matches!(
            ContainerdConfig::new(SOCKET, "default", "  ", "runc", "overlayfs"),
            Err(ContainerdConfigError::ImageRequired)
        ));
    }

    #[test]
    fn accepts_runc_overlayfs_in_a_named_namespace() {
        let _config =
            ContainerdConfig::new(SOCKET, "circus.sandbox-1", "image:tag", "runc", "overlayfs")
                .unwrap();
        #[cfg(unix)]
        {
            assert_eq!(_config.socket(), SOCKET);
            assert_eq!(_config.namespace(), "circus.sandbox-1");
            assert_eq!(_config.image(), "image:tag");
            assert_eq!(_config.snapshotter(), "overlayfs");
        }
    }
}
