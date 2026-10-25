//! A single Codex or Claude Code task over the public Circus sandbox interface.
//!
//! The caller supplies configuration, input bytes and requested artifact paths.
//! [`AgentRunner::start`] returns an owned [`AgentTask`]: consume events while
//! waiting, use `cancel` to stop, and inspect `wait` for the authoritative result.
//! Success is published only after artifacts have been read, the optional
//! workspace archive has been saved, and cleanup succeeds.
//! Dropping the handle requests cancellation; keep it and await `wait` when cleanup
//! must be observed. Runtime/process shutdown is not a durable cleanup guarantee.
//!
//! Sequential Codex turns are caller-sequenced: pass a plaintext, caller-owned
//! [`TaskRequest::workspace`] archive plus the previous
//! [`TaskOutput::agent_session`]. Each task still creates and destroys its own
//! sandbox; this is not in-flight turn recovery.
//!
//! This crate is a caller of `circus-sandbox`; agent protocols and model policy do
//! not belong to the sandbox core. It does not expose HTTP or a Paseo transport.

mod agent;
mod config;
mod events;
mod mcp;
mod model;
mod process;
mod runner;

pub use agent::AgentKind;
pub use config::{ModelConfig, RunnerConfig, TaskRequest, TaskWorkspace};
pub use mcp::{McpConnection, McpGrant, McpToolPolicy};
pub use model::{
    CleanupHandle, ModelFailure, TaskError, TaskEvent, TaskFailure, TaskOutcome, TaskOutput,
    TaskPhase, ToolPhase,
};
pub use runner::{AgentRunner, AgentTask};

#[cfg(test)]
mod tests;
