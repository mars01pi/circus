//! Listener lifecycle, graceful shutdown, and owned-sandbox destruction.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;

use super::config::HttpConfig;
use super::routes;
use super::state::{AppState, DockerSandboxFactory, SandboxFactory};

/// A bound HTTP service.
///
/// Binding resolves an ephemeral port before any request is accepted. Dropping
/// the server without [`HttpServer::serve_with_shutdown`] drops the retained
/// [`crate::Sandbox`] handles, which requests cleanup without confirming it;
/// use graceful shutdown when cleanup must be observed.
pub struct HttpServer {
    state: AppState,
    listener: TcpListener,
    local_addr: SocketAddr,
}

impl HttpServer {
    /// Binds the listener for the validated configuration.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::Bind`] when the operating system rejects the bind
    /// address or the listen call.
    pub async fn bind(config: HttpConfig) -> Result<Self, HttpError> {
        Self::bind_with_factory(config, Arc::new(DockerSandboxFactory)).await
    }

    /// Test seam: binds the same router with a different sandbox factory.
    pub(super) async fn bind_with_factory(
        config: HttpConfig,
        factory: Arc<dyn SandboxFactory>,
    ) -> Result<Self, HttpError> {
        let bind = config.bind_addr();
        let (token, tenants, limits) = config.into_parts();
        let state = AppState::new(token, tenants, factory, limits);
        let listener = TcpListener::bind(bind).await.map_err(HttpError::Bind)?;
        let local_addr = listener.local_addr().map_err(HttpError::Bind)?;
        Ok(Self {
            state,
            listener,
            local_addr,
        })
    }

    /// Actual bound address, including the resolved ephemeral port.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serves until `Ctrl+C`, then destroys every retained sandbox.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::Bind`] only for binding, which happened when the
    /// server was constructed. Runtime serving errors are reported in
    /// [`Served::error`] after cleanup, so cleanup outcomes are never lost.
    pub async fn serve(self) -> Result<Served, HttpError> {
        self.serve_with_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
    }

    /// Serves until `shutdown` resolves, then destroys every retained sandbox.
    ///
    /// Axum first stops accepting connections and drains in-flight requests.
    /// Afterwards every retained handle is destroyed sequentially. A failed
    /// destroy is reported in [`ShutdownReport::failures`] with code
    /// `cleanup_required`; it is never counted as success. Snapshots retained in
    /// memory are dropped.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::Bind`] only for binding, which happened when the
    /// server was constructed. Runtime serving errors are reported in
    /// [`Served::error`] after cleanup.
    pub async fn serve_with_shutdown<F>(self, shutdown: F) -> Result<Served, HttpError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let app = routes::router(self.state.clone());
        let served = axum::serve(self.listener, app)
            .with_graceful_shutdown(shutdown)
            .await;
        let report = destroy_all(&self.state).await;
        Ok(Served {
            report,
            error: served.err(),
        })
    }
}

/// Outcome of a server run. The HTTP response code is not involved: shutdown
/// completeness is only [`ShutdownReport::complete`].
#[derive(Debug)]
pub struct Served {
    pub report: ShutdownReport,
    /// Set when the accept loop failed. Cleanup already ran.
    pub error: Option<std::io::Error>,
}

/// Result of destroying owned sandboxes after graceful shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Sandboxes that were retained when shutdown started.
    pub sandboxes: usize,
    /// Snapshot values dropped from the in-process registries.
    pub snapshots: usize,
    /// One entry per sandbox whose cleanup was not confirmed.
    pub failures: Vec<ShutdownFailure>,
}

impl ShutdownReport {
    /// True only when every retained sandbox reported confirmed absence.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Unconfirmed cleanup for one sandbox during shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownFailure {
    /// Tenant that owned the sandbox.
    pub tenant: String,
    /// Opaque HTTP sandbox identifier.
    pub sandbox_id: String,
    /// Stable error code, currently always `cleanup_required`.
    pub code: &'static str,
}

async fn destroy_all(state: &AppState) -> ShutdownReport {
    let drained = state.drain();
    let sandboxes = drained.sandboxes.len();
    let mut failures = Vec::new();
    for (tenant, sandbox_id, sandbox) in drained.sandboxes {
        if sandbox.destroy().await.is_err() {
            failures.push(ShutdownFailure {
                tenant,
                sandbox_id,
                code: "cleanup_required",
            });
        }
    }
    ShutdownReport {
        sandboxes,
        snapshots: drained.snapshots,
        failures,
    }
}

/// Binding failure.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("cannot bind the HTTP listener: {0}")]
    Bind(#[source] std::io::Error),
}
