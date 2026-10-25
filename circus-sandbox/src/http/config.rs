//! Explicit bind address, bearer token, tenant allow-list, and service limits.

use std::collections::BTreeSet;
use std::net::SocketAddr;

const DEFAULT_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_SNAPSHOT_ENTRIES: usize = 65_536;
const DEFAULT_MAX_SNAPSHOTS_PER_TENANT: usize = 32;

/// Authenticated HTTP service configuration.
///
/// Construction requires an explicit non-empty bearer token and a non-empty
/// tenant allow-list; there is no ambient token, environment lookup, or
/// allow-all mode. The token is owned by the config and zeroed on drop on a
/// best-effort basis.
///
/// # Examples
///
/// ```
/// use std::net::SocketAddr;
/// use circus_sandbox::http::HttpConfig;
///
/// let config = HttpConfig::new(
///     "127.0.0.1:0".parse::<SocketAddr>()?,
///     "correct-horse-battery-staple",
///     ["tenant-a"],
/// )?;
/// assert_eq!(config.bind_addr().port(), 0);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct HttpConfig {
    bind: SocketAddr,
    token: ServiceToken,
    tenants: BTreeSet<String>,
    max_body_bytes: usize,
    snapshot_max_bytes: usize,
    snapshot_max_entries: usize,
    max_snapshots_per_tenant: usize,
}

impl HttpConfig {
    /// Validates an explicit bind address, bearer token, and tenant allow-list.
    ///
    /// A port of `0` asks the operating system to choose an ephemeral port; the
    /// selected address is available from [`crate::http::HttpServer::local_addr`]
    /// after binding. The token must be non-empty visible ASCII without spaces,
    /// so it can be sent as an HTTP bearer credential. Tenant names must be
    /// non-empty visible ASCII of at most 128 bytes. Duplicate tenants are
    /// collapsed.
    ///
    /// # Errors
    ///
    /// Returns [`HttpConfigError`] for an empty or malformed token, no tenants,
    /// or a malformed tenant name.
    pub fn new(
        bind: SocketAddr,
        token: impl Into<Vec<u8>>,
        allowed_tenants: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, HttpConfigError> {
        let token = ServiceToken::new(token.into())?;
        let mut tenants = BTreeSet::new();
        for tenant in allowed_tenants {
            let tenant = tenant.into();
            if tenant.is_empty() {
                return Err(HttpConfigError::EmptyTenant);
            }
            if tenant.len() > 128 || !tenant.bytes().all(visible_ascii) {
                return Err(HttpConfigError::InvalidTenant);
            }
            tenants.insert(tenant);
        }
        if tenants.is_empty() {
            return Err(HttpConfigError::EmptyTenants);
        }
        Ok(Self {
            bind,
            token,
            tenants,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            snapshot_max_bytes: DEFAULT_MAX_SNAPSHOT_BYTES,
            snapshot_max_entries: DEFAULT_MAX_SNAPSHOT_ENTRIES,
            max_snapshots_per_tenant: DEFAULT_MAX_SNAPSHOTS_PER_TENANT,
        })
    }

    /// Replaces the maximum accepted request body size.
    ///
    /// This bounds JSON request bodies and raw file writes. It is not a
    /// container or workspace limit.
    ///
    /// # Errors
    ///
    /// Returns [`HttpConfigError::InvalidBodyLimit`] for `0`.
    pub fn with_max_body_bytes(mut self, bytes: usize) -> Result<Self, HttpConfigError> {
        if bytes == 0 {
            return Err(HttpConfigError::InvalidBodyLimit);
        }
        self.max_body_bytes = bytes;
        Ok(self)
    }

    /// Replaces the bound applied to one snapshot capture request.
    ///
    /// Both bounds must be positive and are checked against the request's
    /// declared capture budget before the domain capture runs.
    ///
    /// # Errors
    ///
    /// Returns [`HttpConfigError::InvalidSnapshotLimits`] if either value is
    /// `0`.
    pub fn with_snapshot_limits(
        mut self,
        max_bytes: usize,
        max_entries: usize,
    ) -> Result<Self, HttpConfigError> {
        if max_bytes == 0 || max_entries == 0 {
            return Err(HttpConfigError::InvalidSnapshotLimits);
        }
        self.snapshot_max_bytes = max_bytes;
        self.snapshot_max_entries = max_entries;
        Ok(self)
    }

    /// Replaces the number of retained snapshots allowed per tenant.
    ///
    /// Capturing past this bound returns `limit_exceeded` without contacting
    /// the sandbox.
    ///
    /// # Errors
    ///
    /// Returns [`HttpConfigError::InvalidSnapshotCount`] for `0`.
    pub fn with_max_snapshots_per_tenant(
        mut self,
        snapshots: usize,
    ) -> Result<Self, HttpConfigError> {
        if snapshots == 0 {
            return Err(HttpConfigError::InvalidSnapshotCount);
        }
        self.max_snapshots_per_tenant = snapshots;
        Ok(self)
    }

    /// Requested bind address, before an ephemeral port is resolved.
    #[must_use]
    pub fn bind_addr(&self) -> SocketAddr {
        self.bind
    }

    pub(super) fn into_parts(self) -> (ServiceToken, BTreeSet<String>, Limits) {
        let limits = self.limits();
        (self.token, self.tenants, limits)
    }

    pub(super) fn limits(&self) -> Limits {
        Limits {
            max_body_bytes: self.max_body_bytes,
            snapshot_max_bytes: self.snapshot_max_bytes,
            snapshot_max_entries: self.snapshot_max_entries,
            max_snapshots_per_tenant: self.max_snapshots_per_tenant,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Limits {
    pub(super) max_body_bytes: usize,
    pub(super) snapshot_max_bytes: usize,
    pub(super) snapshot_max_entries: usize,
    pub(super) max_snapshots_per_tenant: usize,
}

/// Reasons a [`HttpConfig`] value was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HttpConfigError {
    #[error("bearer token must not be empty")]
    EmptyToken,
    #[error("bearer token must be visible ASCII without spaces or control bytes")]
    InvalidToken,
    #[error("at least one allowed tenant is required")]
    EmptyTenants,
    #[error("tenant name must not be empty")]
    EmptyTenant,
    #[error("tenant name must be visible ASCII of at most 128 bytes")]
    InvalidTenant,
    #[error("maximum request body size must be positive")]
    InvalidBodyLimit,
    #[error("snapshot byte and entry limits must be positive")]
    InvalidSnapshotLimits,
    #[error("retained snapshots per tenant must be positive")]
    InvalidSnapshotCount,
}

/// Owned bearer token. Debug output never exposes the bytes.
pub(super) struct ServiceToken(Vec<u8>);

impl ServiceToken {
    fn new(bytes: Vec<u8>) -> Result<Self, HttpConfigError> {
        if bytes.is_empty() {
            return Err(HttpConfigError::EmptyToken);
        }
        if !bytes.iter().copied().all(visible_ascii) {
            return Err(HttpConfigError::InvalidToken);
        }
        Ok(Self(bytes))
    }

    /// Compares without an early exit; the loop covers the shorter operand.
    pub(super) fn matches(&self, presented: &[u8]) -> bool {
        if presented.is_empty() {
            return false;
        }
        let mut difference = u8::from(self.0.len() != presented.len());
        for (expected, supplied) in self.0.iter().zip(presented) {
            difference |= *expected ^ *supplied;
        }
        difference == 0
    }
}

impl std::fmt::Debug for ServiceToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ServiceToken([REDACTED])")
    }
}

impl Drop for ServiceToken {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

fn visible_ascii(byte: u8) -> bool {
    (0x21..=0x7e).contains(&byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address() -> SocketAddr {
        "127.0.0.1:0".parse().expect("address")
    }

    #[test]
    fn rejects_empty_and_whitespace_tokens() {
        assert_eq!(
            HttpConfig::new(address(), Vec::<u8>::new(), ["tenant"]).expect_err("empty token"),
            HttpConfigError::EmptyToken
        );
        assert_eq!(
            HttpConfig::new(address(), "two words", ["tenant"]).expect_err("space"),
            HttpConfigError::InvalidToken
        );
        assert_eq!(
            HttpConfig::new(address(), "token", Vec::<String>::new()).expect_err("tenants"),
            HttpConfigError::EmptyTenants
        );
    }

    #[test]
    fn tenant_allow_list_is_explicit_and_deduplicated() {
        let config = HttpConfig::new(address(), "token", ["a", "a", "b"]).expect("config");
        let (token, tenants, _) = config.into_parts();
        assert_eq!(tenants.len(), 2);
        assert!(token.matches(b"token"));
        assert!(!token.matches(b"Token"));
        assert!(!token.matches(b""));
    }

    #[test]
    fn positive_limits_are_required() {
        let config = HttpConfig::new(address(), "token", ["a"]).expect("config");
        assert_eq!(
            config.with_max_body_bytes(0).expect_err("body limit"),
            HttpConfigError::InvalidBodyLimit
        );
        let config = HttpConfig::new(address(), "token", ["a"]).expect("config");
        assert_eq!(
            config
                .with_snapshot_limits(0, 1)
                .expect_err("snapshot limit"),
            HttpConfigError::InvalidSnapshotLimits
        );
        let config = HttpConfig::new(address(), "token", ["a"]).expect("config");
        assert_eq!(
            config
                .with_max_snapshots_per_tenant(0)
                .expect_err("snapshot count"),
            HttpConfigError::InvalidSnapshotCount
        );
    }
}
