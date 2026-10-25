//! Authenticated HTTP service over the in-process sandbox API.
//!
//! This module is enabled by the default `http` Cargo feature and can be
//! removed from a build with `--no-default-features`. It is a transport
//! adapter, not a second domain implementation: every route calls the same
//! [`crate::Sandbox`] methods as an in-process caller, and the HTTP layer owns
//! no container, workspace, or cleanup state of its own beyond the handles and
//! snapshots it has been asked to retain.
//!
//! # Authentication and tenants
//!
//! Every route, including `GET /healthz`, requires
//! `Authorization: Bearer <token>` and `X-Circus-Tenant: <tenant>`. The token is
//! compared without an early exit on the first differing byte; a missing or
//! wrong token is `401` before route or
//! tenant evaluation, and a tenant outside [`HttpConfig`]'s explicit allow-list
//! is `403`. Error bodies are the stable `{"code", "message"}` pair documented
//! in `docs/http-api.md`; they never contain backend SDK types, host paths, or
//! sandbox instance identities.
//!
//! Each service instance keeps one sandbox registry and one snapshot registry
//! per allowed tenant. Sandbox and snapshot identifiers are random UUIDs, and
//! every lookup is scoped by the authenticated tenant, so a valid identifier
//! from another tenant is indistinguishable from an unknown one (`404`).
//!
//! # Cancellation and cleanup ownership
//!
//! Dropping an in-flight handler future (for example because the client
//! disconnected) drops the awaited domain future. The lifecycle semantics of
//! this crate then apply unchanged: `exec`, file operations, lifecycle
//! transitions, and snapshot operations request whole-instance cleanup when
//! their future is dropped, while `destroy` is never cancelled by a dropped
//! caller. Cleanup ownership stays with the [`crate::Sandbox`] handle retained
//! in the tenant registry; the HTTP layer does not remove a handle merely
//! because a request was cancelled. A later `DELETE` retries cleanup, and
//! graceful server shutdown destroys every retained handle.
//!
//! # Liveness
//!
//! `GET /healthz` reports only that this HTTP process can answer requests. It
//! does not contact Docker, does not inspect images, and is not a readiness,
//! backend-availability, or tenant-capacity signal.
//!
//! # Limits
//!
//! [`HttpConfig`] bounds request bodies, captured snapshot bytes and entries,
//! and retained snapshots per tenant. Those are transport and in-process
//! retention limits, not container resource limits. Container limits come from
//! the [`crate::SandboxConfig`] supplied to `POST /v1/sandboxes`.

mod config;
mod dto;
mod error;
mod routes;
mod server;
mod state;

#[cfg(test)]
mod tests;

pub use config::{HttpConfig, HttpConfigError};
pub use server::{HttpError, HttpServer, Served, ShutdownFailure, ShutdownReport};
