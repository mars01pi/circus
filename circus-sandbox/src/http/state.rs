//! Per-service sandbox and snapshot ownership, scoped by tenant.
//!
//! Locks are held only for map operations. No domain future runs while a lock
//! is held, so a cancelled HTTP handler cannot strand a registry entry.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::api::{BackendConfig, CodeSnapshot, Sandbox, SandboxConfig, SandboxError};

use super::config::{Limits, ServiceToken};

pub(super) type BoxSandboxFuture =
    Pin<Box<dyn Future<Output = Result<Sandbox, SandboxError>> + Send + 'static>>;

/// Creates domain sandboxes. Production uses [`DockerSandboxFactory`]; tests
/// substitute an in-process backend without touching the handlers.
pub(super) trait SandboxFactory: Send + Sync + 'static {
    fn create(&self, backend: BackendConfig, config: SandboxConfig) -> BoxSandboxFuture;
}

pub(super) struct DockerSandboxFactory;

impl SandboxFactory for DockerSandboxFactory {
    fn create(&self, backend: BackendConfig, config: SandboxConfig) -> BoxSandboxFuture {
        Box::pin(Sandbox::create(backend, config))
    }
}

/// Authenticated tenant placed in request extensions by the auth middleware.
#[derive(Debug, Clone)]
pub(super) struct Tenant(pub(super) String);

#[derive(Default)]
struct TenantState {
    sandboxes: HashMap<String, Arc<Sandbox>>,
    snapshots: HashMap<String, Arc<CodeSnapshot>>,
}

struct Inner {
    token: ServiceToken,
    tenants: BTreeSet<String>,
    factory: Arc<dyn SandboxFactory>,
    registries: Mutex<HashMap<String, TenantState>>,
    limits: Limits,
}

/// Shared service state. Clones share one tenant registry.
#[derive(Clone)]
pub(super) struct AppState {
    inner: Arc<Inner>,
}

impl AppState {
    pub(super) fn new(
        token: ServiceToken,
        tenants: BTreeSet<String>,
        factory: Arc<dyn SandboxFactory>,
        limits: Limits,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                token,
                tenants,
                factory,
                registries: Mutex::new(HashMap::new()),
                limits,
            }),
        }
    }

    pub(super) fn token(&self) -> &ServiceToken {
        &self.inner.token
    }

    pub(super) fn tenant_allowed(&self, tenant: &str) -> bool {
        self.inner.tenants.contains(tenant)
    }

    pub(super) fn max_body_bytes(&self) -> usize {
        self.inner.limits.max_body_bytes
    }

    pub(super) fn snapshot_max_bytes(&self) -> usize {
        self.inner.limits.snapshot_max_bytes
    }

    pub(super) fn snapshot_max_entries(&self) -> usize {
        self.inner.limits.snapshot_max_entries
    }

    /// Creates the sandbox first, then records it under the tenant.
    pub(super) async fn create_sandbox(
        &self,
        tenant: &str,
        backend: BackendConfig,
        config: SandboxConfig,
    ) -> Result<(String, Arc<Sandbox>), SandboxError> {
        let sandbox = self.inner.factory.create(backend, config).await?;
        let id = uuid::Uuid::new_v4().to_string();
        let sandbox = Arc::new(sandbox);
        self.with_tenant(tenant, |state| {
            state.sandboxes.insert(id.clone(), Arc::clone(&sandbox));
        });
        Ok((id, sandbox))
    }

    /// Tenant-scoped lookup. Another tenant's valid identifier is absent.
    pub(super) fn sandbox(&self, tenant: &str, id: &str) -> Option<Arc<Sandbox>> {
        self.with_tenant(tenant, |state| state.sandboxes.get(id).cloned())
    }

    /// Removes only the exact handle that confirmed cleanup. This keeps a
    /// retryable `CleanupRequired` handle registered.
    pub(super) fn remove_sandbox(&self, tenant: &str, id: &str, sandbox: &Arc<Sandbox>) {
        self.with_tenant(tenant, |state| {
            if state
                .sandboxes
                .get(id)
                .is_some_and(|current| Arc::ptr_eq(current, sandbox))
            {
                state.sandboxes.remove(id);
            }
        });
    }

    /// Retains one captured snapshot under a random identifier. `None` means
    /// the per-tenant retention bound is reached.
    pub(super) fn retain_snapshot(
        &self,
        tenant: &str,
        snapshot: Arc<CodeSnapshot>,
    ) -> Option<String> {
        self.with_tenant(tenant, |state| {
            if state.snapshots.len() >= self.inner.limits.max_snapshots_per_tenant {
                None
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                state.snapshots.insert(id.clone(), snapshot);
                Some(id)
            }
        })
    }

    pub(super) fn snapshot(&self, tenant: &str, snapshot_id: &str) -> Option<Arc<CodeSnapshot>> {
        self.with_tenant(tenant, |state| state.snapshots.get(snapshot_id).cloned())
    }

    /// Takes every retained handle so shutdown can destroy them.
    pub(super) fn drain(&self) -> Drained {
        let mut registries = self.registries();
        let mut drained = Drained::default();
        for (tenant, state) in registries.drain() {
            drained.sandboxes.extend(
                state
                    .sandboxes
                    .into_iter()
                    .map(|(id, sandbox)| (tenant.clone(), id, sandbox)),
            );
            drained.snapshots += state.snapshots.len();
        }
        drained
    }

    fn with_tenant<T>(&self, tenant: &str, operation: impl FnOnce(&mut TenantState) -> T) -> T {
        let mut registries = self.registries();
        operation(registries.entry(tenant.to_string()).or_default())
    }

    fn registries(&self) -> MutexGuard<'_, HashMap<String, TenantState>> {
        self.inner
            .registries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Default)]
pub(super) struct Drained {
    pub(super) sandboxes: Vec<(String, String, Arc<Sandbox>)>,
    pub(super) snapshots: usize,
}
