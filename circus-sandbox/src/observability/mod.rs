//! Caller-owned counters and stable, privacy-bounded tracing events.
//!
//! This module adds two independent, opt-in observation surfaces:
//!
//! - [`SandboxMetrics`] is a small cloneable handle over a fixed family of
//!   atomic counters. The caller creates it and passes it explicitly to
//!   [`crate::Sandbox::create_with_metrics`] or
//!   [`crate::PoolConfig::with_metrics`]. Without a handle, recording is a
//!   branch-only no-op; no global registry is installed and nothing is exported.
//! - The crate-private `events` module emits `tracing` events and spans with
//!   stable names and small typed fields. This crate never installs a global
//!   subscriber; the embedding application decides whether events are
//!   collected and where they go.
//!
//! Event fields contain only fixed labels, durations, counts, booleans, and the
//! program basename of a command. They never contain environment values, full
//! argv, guest file contents, host paths, image credentials, tenant secrets, or
//! raw endpoint URLs with credentials.
//!
//! This is instrumentation, not a health service. The crate exposes no health
//! or readiness endpoint and makes no liveness claim; an unavailable capability
//! (`UnsupportedCapability`) and cleanup that is still pending are distinct
//! event field values and distinct counters.

pub(crate) mod events;
mod metrics;

pub use metrics::{
    MetricsSnapshot, OperationCounters, OperationKind, OperationOutcome, SandboxMetrics,
    TerminationKind,
};

#[cfg(test)]
mod tests;
