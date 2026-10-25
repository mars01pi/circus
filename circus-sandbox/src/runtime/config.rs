//! Backend-neutral sandbox configuration and the backend selection descriptor.

use std::time::Duration;

use super::{ResourceLimits, ResourceLimitsError};

/// Backend selection for [`crate::Sandbox::create`].
///
/// Three variants are implemented. The enum name is stable so a future backend
/// can be added without changing the creation entry point; adding a variant is
/// the only way to claim another backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendConfig {
    /// A local Docker Engine endpoint running a Linux image with `runc`.
    ///
    /// See [`DockerBackendConfig`]. Selecting this variant does not assert that
    /// another backend exists, that the image is present, or that the engine is
    /// reachable: [`crate::Sandbox::create`] validates and prepares it.
    Docker(DockerBackendConfig),
    /// A local containerd socket running a Linux image with `runc` through the
    /// OCI runtime handler.
    ///
    /// See [`ContainerdBackendConfig`]. Selecting this variant does not assert
    /// that the image is present or unpacked, that the daemon is reachable, or
    /// that the requested snapshotter is configured: [`crate::Sandbox::create`]
    /// validates and prepares it. `runsc` (gVisor) is rejected as unavailable.
    Containerd(ContainerdBackendConfig),
    /// A Kubernetes namespace running a Linux image as a Pod with `runc` on
    /// the node.
    ///
    /// See [`KubernetesBackendConfig`]. Selecting this variant does not assert
    /// that the API server is reachable, that the namespace exists, or that the
    /// image is cached on a node: [`crate::Sandbox::create`] validates and
    /// prepares it. Requires the `kubernetes` crate feature; without it,
    /// creation reports an unsupported capability.
    Kubernetes(KubernetesBackendConfig),
}

impl BackendConfig {
    /// Builds an explicit Docker backend descriptor.
    ///
    /// `endpoint` must name an authorized local Unix socket or Windows named
    /// pipe. `image` must already exist in the local daemon; this crate never
    /// pulls. `runtime` must be `runc`. The descriptor is validated before any
    /// container allocation; invalid values are reported by
    /// [`crate::Sandbox::create`] as [`BackendConfigError`].
    #[must_use]
    pub fn docker(
        endpoint: impl Into<String>,
        image: impl Into<String>,
        runtime: impl Into<String>,
    ) -> Self {
        Self::Docker(DockerBackendConfig::new(endpoint, image, runtime))
    }

    /// Builds an explicit containerd backend descriptor.
    ///
    /// `socket` must be an absolute path to a Unix socket. `namespace` scopes
    /// every containerd request. `image` must already exist in that namespace;
    /// sandbox creation never pulls. `runtime` must be `runc`, and `snapshotter` must
    /// be `overlayfs`. The descriptor is validated before any container
    /// allocation; invalid values are reported by [`crate::Sandbox::create`]
    /// as [`BackendConfigError`].
    #[must_use]
    pub fn containerd(
        socket: impl Into<String>,
        namespace: impl Into<String>,
        image: impl Into<String>,
        runtime: impl Into<String>,
        snapshotter: impl Into<String>,
    ) -> Self {
        Self::Containerd(ContainerdBackendConfig::new(
            socket,
            namespace,
            image,
            runtime,
            snapshotter,
        ))
    }

    /// Builds an explicit Kubernetes workload descriptor.
    ///
    /// `kubeconfig` must be an absolute path to a kubeconfig file; ambient
    /// `KUBECONFIG` and `$HOME/.kube/config` are never consulted. `namespace`
    /// is a DNS-1123 label and `image` must be a lowercase image reference.
    /// The descriptor is validated before any Pod allocation; invalid values
    /// are reported by [`crate::Sandbox::create`] as [`BackendConfigError`].
    /// Use [`KubernetesBackendConfig::with_context`] or
    /// [`KubernetesBackendConfig::with_service_account`] for the optional
    /// values.
    #[must_use]
    pub fn kubernetes(
        kubeconfig: impl Into<String>,
        namespace: impl Into<String>,
        image: impl Into<String>,
    ) -> Self {
        Self::Kubernetes(KubernetesBackendConfig::new(kubeconfig, namespace, image))
    }
}

/// Docker backend descriptor: local endpoint, local image, OCI runtime name.
///
/// Plain strings preserve the descriptor as data; parsing and validation are
/// backend details performed during creation. No registry credentials, Docker
/// context, or ambient authentication are part of this value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerBackendConfig {
    endpoint: String,
    image: String,
    runtime: String,
}

impl DockerBackendConfig {
    /// Stores the supplied descriptor values without connecting to Docker.
    #[must_use]
    pub fn new(
        endpoint: impl Into<String>,
        image: impl Into<String>,
        runtime: impl Into<String>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            image: image.into(),
            runtime: runtime.into(),
        }
    }

    /// Authorized local endpoint, for example `unix:///var/run/docker.sock`.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Local image reference or immutable image ID. Never pulled.
    #[must_use]
    pub fn image(&self) -> &str {
        &self.image
    }

    /// Requested OCI runtime name.
    #[must_use]
    pub fn runtime(&self) -> &str {
        &self.runtime
    }
}

/// containerd backend descriptor: local socket, namespace, image, OCI runtime
/// handler source, and snapshotter.
///
/// Plain strings preserve the descriptor as data; parsing and validation are
/// backend details performed during creation. No registry credentials,
/// containerd context, or ambient namespace are part of this value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerdBackendConfig {
    socket: String,
    namespace: String,
    image: String,
    runtime: String,
    snapshotter: String,
}

impl ContainerdBackendConfig {
    /// Stores the supplied descriptor values without connecting to containerd.
    #[must_use]
    pub fn new(
        socket: impl Into<String>,
        namespace: impl Into<String>,
        image: impl Into<String>,
        runtime: impl Into<String>,
        snapshotter: impl Into<String>,
    ) -> Self {
        Self {
            socket: socket.into(),
            namespace: namespace.into(),
            image: image.into(),
            runtime: runtime.into(),
            snapshotter: snapshotter.into(),
        }
    }

    /// Absolute Unix socket path, for example `/run/containerd/containerd.sock`.
    #[must_use]
    pub fn socket(&self) -> &str {
        &self.socket
    }

    /// containerd namespace scoping every request.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Local image reference in that namespace. Never pulled.
    #[must_use]
    pub fn image(&self) -> &str {
        &self.image
    }

    /// Requested OCI runtime name. Only `runc` is implemented; `runsc` is
    /// unavailable on this host.
    #[must_use]
    pub fn runtime(&self) -> &str {
        &self.runtime
    }

    /// Requested snapshotter name. Only `overlayfs` is implemented.
    #[must_use]
    pub fn snapshotter(&self) -> &str {
        &self.snapshotter
    }
}

/// Kubernetes workload descriptor: explicit kubeconfig, namespace, image, and
/// optional context and service account.
///
/// Plain strings preserve the descriptor as data; parsing and validation are
/// performed during creation. Credentials are read only from `kubeconfig`
/// (never ambient `KUBECONFIG`, `$HOME/.kube/config`, or an in-cluster service
/// account token). The image is not pulled by this crate: the node's kubelet
/// pulls it when absent under `imagePullPolicy: IfNotPresent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KubernetesBackendConfig {
    kubeconfig: String,
    context: Option<String>,
    namespace: String,
    image: String,
    service_account: Option<String>,
}

impl KubernetesBackendConfig {
    /// Stores the supplied descriptor values without reading the kubeconfig.
    #[must_use]
    pub fn new(
        kubeconfig: impl Into<String>,
        namespace: impl Into<String>,
        image: impl Into<String>,
    ) -> Self {
        Self {
            kubeconfig: kubeconfig.into(),
            context: None,
            namespace: namespace.into(),
            image: image.into(),
            service_account: None,
        }
    }

    /// Selects a named kubeconfig context instead of the file's current
    /// context.
    #[must_use]
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    /// Runs the Pod under this service account instead of the namespace
    /// default.
    #[must_use]
    pub fn with_service_account(mut self, service_account: impl Into<String>) -> Self {
        self.service_account = Some(service_account.into());
        self
    }

    /// Absolute kubeconfig path. Never resolved from ambient environment.
    #[must_use]
    pub fn kubeconfig(&self) -> &str {
        &self.kubeconfig
    }

    /// Optional named context in that kubeconfig.
    #[must_use]
    pub fn context(&self) -> Option<&str> {
        self.context.as_deref()
    }

    /// Namespace scoping every Kubernetes request.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Image reference. The node's kubelet pulls it with
    /// `imagePullPolicy: IfNotPresent`.
    #[must_use]
    pub fn image(&self) -> &str {
        &self.image
    }

    /// Optional service account for the Pod.
    #[must_use]
    pub fn service_account(&self) -> Option<&str> {
        self.service_account.as_deref()
    }
}

/// Isolation requirements that must all be satisfied by every backend.
///
/// There is no insecure constructor. A backend maps these requirements to its
/// own mechanisms and must reject a configuration it cannot satisfy.
///
/// The Docker backend interprets them as: a read-only container root
/// filesystem, `no-new-privileges`, every Linux capability dropped, a private
/// (disabled) network namespace, and the requested workspace path provided as
/// a bounded tmpfs instead of a host bind. The containerd backend interprets
/// them as the same OCI spec properties: a read-only root filesystem, an empty
/// capability set with `noNewPrivileges`, a new network namespace with no
/// interfaces, and a bounded tmpfs at the requested workspace path. These
/// interpretations live in the backend adapter; the values here are
/// requirements only, not evidence that a running container achieved them. Use
/// [`crate::SandboxStatus::enforcement`] for inspected evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationConfig {
    workspace_path: String,
    guest_uid: u32,
    guest_gid: u32,
}

impl IsolationConfig {
    /// Builds a Linux isolation policy.
    ///
    /// `workspace_path` is an absolute container path, never a host path. It is
    /// the guest-visible workload directory; the backend must not expose a host
    /// bind there. The Docker backend mounts a bounded tmpfs at that path.
    ///
    /// # Errors
    ///
    /// Rejects any disabled flag, a non-absolute workspace path, or a path that
    /// contains `..`, `\`, or a Windows drive prefix in any component.
    pub fn linux_enforced(
        no_new_privileges: bool,
        drop_all_capabilities: bool,
        readonly_root: bool,
        network_disabled: bool,
        workspace_path: impl Into<String>,
        guest_uid: u32,
        guest_gid: u32,
    ) -> Result<Self, ConfigError> {
        if !no_new_privileges || !drop_all_capabilities || !readonly_root || !network_disabled {
            return Err(ConfigError::IsolationNotEnforced);
        }
        let workspace_path = workspace_path.into();
        check_container_dir(&workspace_path)?;
        Ok(Self {
            workspace_path,
            guest_uid,
            guest_gid,
        })
    }

    /// Absolute guest workspace path.
    #[must_use]
    pub fn workspace_path(&self) -> &str {
        &self.workspace_path
    }

    /// Guest user id. Passed explicitly to the backend; not inherited.
    #[must_use]
    pub fn guest_uid(&self) -> u32 {
        self.guest_uid
    }

    /// Guest group id.
    #[must_use]
    pub fn guest_gid(&self) -> u32 {
        self.guest_gid
    }
}

/// Explicit bounds for captured output and operation deadlines.
///
/// Every deadline is positive and applies to one operation class. They are not
/// interchangeable: one value is never silently reused for a different class.
///
/// - [`Self::command_timeout`] is the ceiling for one `exec` operation. The
///   worker applies the smaller of the request deadline and this ceiling; a
///   command deadline destroys the owned instance because the guest process
///   may have outlived its result.
/// - [`Self::file_timeout`] bounds guest file operations (`read_file`,
///   `write_file`, `create_dir_all`, `remove_file`) and workspace snapshot
///   capture/restore. Their timeout also destroys the owned instance because an
///   in-flight transport operation cannot be resumed safely.
/// - [`Self::lifecycle_timeout`] bounds `start`, `pause`, and `resume`, and
///   also `status` inspection. A timed-out lifecycle transition destroys the
///   owned instance because the engine may already have applied the transition.
///   A timed-out `status` observation does **not** destroy anything: it is a
///   read-only inspection and leaves the phase, identity, and ownership
///   retryable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoLimits {
    output_limit_bytes: usize,
    command_timeout: Duration,
    file_timeout: Duration,
    lifecycle_timeout: Duration,
}

impl IoLimits {
    /// Validates shared output and file-operation bounds using one deadline for
    /// all operation classes.
    ///
    /// Use [`Self::with_deadlines`] when command, file, and lifecycle/status
    /// deadlines must differ. This constructor stores the supplied file deadline
    /// explicitly as the command and lifecycle/status deadline as well; it does
    /// not leave any class unset.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidOutputLimit`] for `0` or `usize::MAX` and
    /// [`ConfigError::InvalidFileTimeout`] for a zero file deadline.
    pub fn new(output_limit_bytes: usize, file_timeout: Duration) -> Result<Self, ConfigError> {
        Self::with_deadlines(output_limit_bytes, file_timeout, file_timeout, file_timeout)
    }

    /// Validates a distinct positive deadline for each operation class.
    ///
    /// `command_timeout` caps one exec, `file_timeout` bounds guest file
    /// transport and snapshots, and `lifecycle_timeout` bounds state
    /// transitions plus read-only status inspection. See the type documentation
    /// for which timeouts destroy the instance and which leave it owned.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidOutputLimit`] for `0` or `usize::MAX`,
    /// [`ConfigError::InvalidFileTimeout`] for a zero file deadline,
    /// [`ConfigError::InvalidCommandTimeout`] for a zero command deadline, and
    /// [`ConfigError::InvalidLifecycleTimeout`] for a zero lifecycle/status
    /// deadline.
    pub fn with_deadlines(
        output_limit_bytes: usize,
        command_timeout: Duration,
        file_timeout: Duration,
        lifecycle_timeout: Duration,
    ) -> Result<Self, ConfigError> {
        if output_limit_bytes == 0 || output_limit_bytes == usize::MAX {
            return Err(ConfigError::InvalidOutputLimit { output_limit_bytes });
        }
        if file_timeout.is_zero() {
            return Err(ConfigError::InvalidFileTimeout);
        }
        if command_timeout.is_zero() {
            return Err(ConfigError::InvalidCommandTimeout);
        }
        if lifecycle_timeout.is_zero() {
            return Err(ConfigError::InvalidLifecycleTimeout);
        }
        Ok(Self {
            output_limit_bytes,
            command_timeout,
            file_timeout,
            lifecycle_timeout,
        })
    }

    /// Shared stdout+stderr byte bound for one exec request.
    #[must_use]
    pub fn output_limit_bytes(&self) -> usize {
        self.output_limit_bytes
    }

    /// Ceiling applied to every exec operation.
    #[must_use]
    pub fn command_timeout(&self) -> Duration {
        self.command_timeout
    }

    /// Deadline for guest file operations and workspace snapshots.
    #[must_use]
    pub fn file_timeout(&self) -> Duration {
        self.file_timeout
    }

    /// Deadline for lifecycle transitions and read-only status inspection.
    #[must_use]
    pub fn lifecycle_timeout(&self) -> Duration {
        self.lifecycle_timeout
    }
}

/// Create-time configuration shared by all backends.
///
/// Backend-specific values (endpoint, image, runtime) live in
/// [`BackendConfig`]. No field is filled from a daemon, image, or host default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxConfig {
    limits: ResourceLimits,
    isolation: IsolationConfig,
    io_limits: IoLimits,
}

impl SandboxConfig {
    /// Validates a complete backend-neutral configuration.
    ///
    /// Each create call generates its own ownership nonce before requesting an
    /// instance. Passing validation does not reserve resources, perform
    /// aggregate admission, or prove that a backend enforces the request.
    /// Backend-specific constraints are validated by the selected backend
    /// before allocation; the Docker backend, for example, requires at least
    /// three PIDs for its init/supervisor/command structure.
    ///
    /// # Errors
    ///
    /// Returns a typed resource-limit error if the nested values are invalid.
    /// Values stored in [`ResourceLimits`] and [`IsolationConfig`] were already
    /// validated, so this constructor currently cannot fail; it remains
    /// fallible so future cross-value requirements can be added without a
    /// breaking signature change.
    pub fn new(
        limits: ResourceLimits,
        isolation: IsolationConfig,
        io_limits: IoLimits,
    ) -> Result<Self, ConfigError> {
        Ok(Self {
            limits,
            isolation,
            io_limits,
        })
    }

    #[must_use]
    pub fn limits(&self) -> ResourceLimits {
        self.limits
    }

    #[must_use]
    pub fn isolation(&self) -> &IsolationConfig {
        &self.isolation
    }

    #[must_use]
    pub fn output_limit_bytes(&self) -> usize {
        self.io_limits.output_limit_bytes
    }

    /// Ceiling for one exec operation; the worker uses the smaller of this
    /// value and the per-request deadline.
    #[must_use]
    pub fn command_timeout(&self) -> Duration {
        self.io_limits.command_timeout
    }

    /// Deadline for guest file operations and workspace snapshots.
    #[must_use]
    pub fn file_timeout(&self) -> Duration {
        self.io_limits.file_timeout
    }

    /// Deadline for lifecycle transitions and read-only status inspection. A
    /// status timeout does not destroy the instance.
    #[must_use]
    pub fn lifecycle_timeout(&self) -> Duration {
        self.io_limits.lifecycle_timeout
    }
}

/// Why a backend-neutral configuration value was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "no-new-privileges, capability drop, readonly root, and disabled network must all be set"
    )]
    IsolationNotEnforced,
    #[error("workspace path must be an absolute container path without host prefixes")]
    InvalidWorkspacePath,
    #[error("output_limit_bytes must be in 1..usize::MAX, got {output_limit_bytes}")]
    InvalidOutputLimit { output_limit_bytes: usize },
    #[error("file-operation timeout must be non-zero")]
    InvalidFileTimeout,
    #[error("command timeout must be non-zero")]
    InvalidCommandTimeout,
    #[error("lifecycle/status timeout must be non-zero")]
    InvalidLifecycleTimeout,
    #[error(transparent)]
    Limits(#[from] ResourceLimitsError),
    #[error("relative path rejected: {reason}")]
    RelativePath { reason: &'static str },
}

/// Why a backend descriptor was rejected before any instance allocation.
///
/// Variants are backend-defined but neutral in representation, so callers can
/// match them without importing a backend SDK or its configuration type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackendConfigError {
    #[error("endpoint is not an authorized local unix socket or npipe: {value}")]
    UnsupportedEndpoint { value: String },
    #[error("runtime {name} is not supported; only runc is accepted")]
    UnsupportedRuntime { name: String },
    #[error("runtime {name} is not available on this host; only runc is implemented")]
    RuntimeUnavailable { name: String },
    #[error("snapshotter {name} is not supported; only overlayfs is implemented")]
    UnsupportedSnapshotter { name: String },
    #[error("containerd socket must be an absolute unix socket path: {value}")]
    InvalidSocketPath { value: String },
    #[error(
        "containerd namespace must be nonempty and use only ASCII alphanumerics, '.', '_' or '-': {value}"
    )]
    InvalidNamespace { value: String },
    #[error("image reference is required and is not pulled by this crate")]
    ImageRequired,
    #[error("kubeconfig must be an absolute file path: {value}")]
    InvalidKubeconfigPath { value: String },
    #[error("Kubernetes namespace must be a DNS-1123 label: {value}")]
    InvalidKubernetesNamespace { value: String },
    #[error("Kubernetes context name must be nonempty and at most 253 characters: {value}")]
    InvalidContextName { value: String },
    #[error("Kubernetes service account must be a DNS-1123 subdomain: {value}")]
    InvalidServiceAccount { value: String },
    #[error("image reference is not a valid Docker reference: {value}")]
    InvalidImageReference { value: String },
    #[error("Kubernetes runAsNonRoot requires a non-zero guest uid, got {uid}")]
    NonRootGuestRequired { uid: u32 },
    #[error("pids {pids} cannot fit init, supervisor and command; minimum is 3")]
    SupervisorNeedsPid { pids: u32 },
}

pub(crate) fn check_container_dir(path: &str) -> Result<(), ConfigError> {
    if !path.starts_with('/') || path.contains('\\') || path.starts_with("//") {
        return Err(ConfigError::InvalidWorkspacePath);
    }
    if path
        .strip_prefix('/')
        .is_none_or(|rest| rest.is_empty() || rest.split('/').any(bad_component))
    {
        return Err(ConfigError::InvalidWorkspacePath);
    }
    Ok(())
}

pub(crate) fn check_relative(path: &str) -> Result<(), ConfigError> {
    if path.is_empty() {
        return Err(ConfigError::RelativePath { reason: "empty" });
    }
    if path.contains('\\') || path.starts_with('/') {
        return Err(ConfigError::RelativePath {
            reason: "not a relative slash path",
        });
    }
    if path.split('/').any(bad_component) {
        return Err(ConfigError::RelativePath {
            reason: "rejected component",
        });
    }
    Ok(())
}

fn bad_component(component: &str) -> bool {
    component.is_empty() || component == "." || component == ".." || drive_prefix(component)
}

fn drive_prefix(component: &str) -> bool {
    let bytes = component.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_insecure_isolation_and_drive_components() {
        assert!(matches!(
            IsolationConfig::linux_enforced(false, true, true, true, "/workspace", 1000, 1000),
            Err(ConfigError::IsolationNotEnforced)
        ));
        assert!(check_relative("notes/C:secret").is_err());
        assert!(check_relative("notes/hello.txt").is_ok());
        assert!(check_container_dir("workspace").is_err());
        assert!(check_container_dir("/workspace").is_ok());
        assert!(check_container_dir("/../workspace").is_err());
    }

    #[test]
    fn neutral_config_exposes_typed_values() {
        let limits = ResourceLimits::new(1024, 100, 3, 512, 128).unwrap();
        let isolation =
            IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 1000, 1000)
                .unwrap();
        let io = IoLimits::new(4096, Duration::from_secs(2)).unwrap();
        let config = SandboxConfig::new(limits, isolation, io).unwrap();
        assert_eq!(config.limits(), limits);
        assert_eq!(config.isolation().workspace_path(), "/workspace");
        assert_eq!(config.output_limit_bytes(), 4096);
        assert_eq!(config.file_timeout(), Duration::from_secs(2));
        assert_eq!(config.command_timeout(), Duration::from_secs(2));
        assert_eq!(config.lifecycle_timeout(), Duration::from_secs(2));
    }

    #[test]
    fn separate_operation_deadlines_are_explicit_and_positive() {
        let io = IoLimits::with_deadlines(
            4096,
            Duration::from_secs(4),
            Duration::from_secs(2),
            Duration::from_secs(9),
        )
        .unwrap();
        assert_eq!(io.command_timeout(), Duration::from_secs(4));
        assert_eq!(io.file_timeout(), Duration::from_secs(2));
        assert_eq!(io.lifecycle_timeout(), Duration::from_secs(9));
        assert_eq!(
            IoLimits::with_deadlines(
                1,
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(1)
            ),
            Err(ConfigError::InvalidCommandTimeout)
        );
        assert_eq!(
            IoLimits::with_deadlines(
                1,
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::ZERO
            ),
            Err(ConfigError::InvalidLifecycleTimeout)
        );
        assert_eq!(
            IoLimits::new(1, Duration::ZERO),
            Err(ConfigError::InvalidFileTimeout)
        );
    }

    #[test]
    fn backend_descriptor_is_plain_data() {
        let backend = BackendConfig::docker("unix:///var/run/docker.sock", "alpine:3.20", "runc");
        match backend {
            BackendConfig::Docker(docker) => {
                assert_eq!(docker.endpoint(), "unix:///var/run/docker.sock");
                assert_eq!(docker.image(), "alpine:3.20");
                assert_eq!(docker.runtime(), "runc");
            }
            BackendConfig::Containerd(_) => panic!("expected a Docker descriptor"),
            BackendConfig::Kubernetes(_) => panic!("expected a Docker descriptor"),
        }
    }

    #[test]
    fn containerd_descriptor_is_plain_data() {
        let backend = BackendConfig::containerd(
            "/run/containerd/containerd.sock",
            "circus",
            "python:3.12-slim",
            "runc",
            "overlayfs",
        );
        match backend {
            BackendConfig::Containerd(containerd) => {
                assert_eq!(containerd.socket(), "/run/containerd/containerd.sock");
                assert_eq!(containerd.namespace(), "circus");
                assert_eq!(containerd.image(), "python:3.12-slim");
                assert_eq!(containerd.runtime(), "runc");
                assert_eq!(containerd.snapshotter(), "overlayfs");
            }
            BackendConfig::Docker(_) => panic!("expected a containerd descriptor"),
            BackendConfig::Kubernetes(_) => panic!("expected a containerd descriptor"),
        }
    }

    #[test]
    fn kubernetes_descriptor_is_plain_data() {
        let backend = BackendConfig::kubernetes(
            "/etc/kubernetes/kubeconfig",
            "sandboxes",
            "docker.io/library/python:3.12-slim",
        );
        let BackendConfig::Kubernetes(kubernetes) = backend else {
            panic!("expected a Kubernetes descriptor");
        };
        assert_eq!(kubernetes.kubeconfig(), "/etc/kubernetes/kubeconfig");
        assert_eq!(kubernetes.namespace(), "sandboxes");
        assert_eq!(kubernetes.image(), "docker.io/library/python:3.12-slim");
        assert_eq!(kubernetes.context(), None);
        assert_eq!(kubernetes.service_account(), None);

        let kubernetes = KubernetesBackendConfig::new(
            "/etc/kubernetes/kubeconfig",
            "sandboxes",
            "python:3.12-slim",
        )
        .with_context("circus")
        .with_service_account("sandbox");
        assert_eq!(kubernetes.context(), Some("circus"));
        assert_eq!(kubernetes.service_account(), Some("sandbox"));
    }
}
