//! Stable `tracing` event and span names for lifecycle, pool, and backend work.
//!
//! Every event carries an `event` field whose value is a fixed name. Fields are
//! limited to counters, booleans, durations, fixed labels, and a sanitized
//! command program basename. Environment values, full argv, file contents, host
//! paths, image credentials, and endpoint URLs are never recorded.

use std::collections::BTreeMap;

/// The sanitized, bounded facts that may cross into telemetry for one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandFacts {
    /// Basename of `argv[0]`, filtered to printable ASCII and capped at 64 bytes.
    pub program: String,
    /// Number of argv elements, including the executable.
    pub argc: usize,
    /// Number of environment overrides, never their names or values.
    pub env_overrides: usize,
}

/// Extracts the only command properties telemetry may contain.
pub(crate) fn command_facts(argv: &[String], env_overrides: usize) -> CommandFacts {
    let raw = argv.first().map_or("", String::as_str);
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let program: String = base
        .chars()
        .filter(|c| c.is_ascii_graphic() && *c != '=')
        .take(64)
        .collect();
    CommandFacts {
        program: if program.is_empty() {
            "program".to_string()
        } else {
            program
        },
        argc: argv.len(),
        env_overrides,
    }
}

/// Extracts command facts from a process request without reading values.
pub(crate) fn process_facts(argv: &[String], env: &BTreeMap<String, String>) -> CommandFacts {
    command_facts(argv, env.len())
}

/// Opens the `sandbox.create` span.
pub(crate) fn create_span(backend: &'static str) -> tracing::Span {
    tracing::info_span!(target: "circus_sandbox", "sandbox.create", backend)
}

/// A creation attempt entered backend validation or preparation.
pub(crate) fn create_started(backend: &'static str) {
    tracing::info!(target: "circus_sandbox", event = "sandbox.create.started", backend);
}

/// A creation attempt settled.
///
/// `cleanup_pending` is `true` exactly when the owned instance or staging is
/// still owed cleanup, which is distinct from an unavailable capability.
pub(crate) fn create_completed(
    outcome: &'static str,
    cleanup_pending: bool,
    error_kind: &'static str,
) {
    tracing::info!(
        target: "circus_sandbox",
        event = "sandbox.create.completed",
        outcome = outcome,
        cleanup_pending = cleanup_pending,
        error_kind = error_kind,
    );
}

/// The selected backend cannot honor a requested capability.
pub(crate) fn capability_unavailable(capability: &'static str) {
    tracing::warn!(
        target: "circus_sandbox",
        event = "sandbox.capability.unavailable",
        capability = capability,
    );
}

/// A start, pause, or resume call settled.
pub(crate) fn transition_completed(
    operation: &'static str,
    outcome: &'static str,
    cleanup_required: bool,
) {
    tracing::info!(
        target: "circus_sandbox",
        event = "sandbox.transition.completed",
        operation = operation,
        outcome = outcome,
        cleanup_required = cleanup_required,
    );
}

/// A one-shot exec call settled.
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_completed(
    facts: &CommandFacts,
    duration_ms: u64,
    outcome: &'static str,
    exit_code: Option<i64>,
    stdout_bytes: usize,
    stderr_bytes: usize,
    output_limited: bool,
    output_stream: Option<&'static str>,
) {
    tracing::info!(
        target: "circus_sandbox",
        event = "sandbox.exec.completed",
        program = facts.program.as_str(),
        argc = count(facts.argc),
        duration_ms = duration_ms,
        outcome = outcome,
        exit_code = ?exit_code,
        stdout_bytes = count(stdout_bytes),
        stderr_bytes = count(stderr_bytes),
        output_limited = output_limited,
        output_stream = output_stream.unwrap_or("none"),
    );
}

/// One interactive process session attached.
pub(crate) fn process_opened(facts: &CommandFacts, timeout_ms: u64, output_limit_bytes: usize) {
    tracing::info!(
        target: "circus_sandbox",
        event = "sandbox.process.opened",
        program = facts.program.as_str(),
        argc = count(facts.argc),
        env_overrides = count(facts.env_overrides),
        timeout_ms = timeout_ms,
        output_limit_bytes = count(output_limit_bytes),
    );
}

/// One interactive process session ended with its termination cause.
pub(crate) fn process_closed(
    reason: &'static str,
    exit_code: Option<i64>,
    ownership: Option<&'static str>,
) {
    tracing::info!(
        target: "circus_sandbox",
        event = "sandbox.process.closed",
        reason = reason,
        exit_code = ?exit_code,
        ownership = ownership.unwrap_or("not_applicable"),
    );
}

/// A workspace snapshot capture settled.
pub(crate) fn snapshot_capture_completed(outcome: &'static str, cleanup_required: bool) {
    tracing::info!(
        target: "circus_sandbox",
        event = "sandbox.snapshot.capture.completed",
        outcome = outcome,
        cleanup_required = cleanup_required,
    );
}

/// A workspace snapshot restore settled.
pub(crate) fn snapshot_restore_completed(outcome: &'static str, cleanup_required: bool) {
    tracing::info!(
        target: "circus_sandbox",
        event = "sandbox.snapshot.restore.completed",
        outcome = outcome,
        cleanup_required = cleanup_required,
    );
}

/// One cleanup attempt finished with an observed ownership result.
pub(crate) fn cleanup_attempted(ownership: &'static str, complete: bool, creation_cleanup: bool) {
    tracing::info!(
        target: "circus_sandbox",
        event = "sandbox.cleanup.attempted",
        ownership = ownership,
        complete = complete,
        creation_cleanup = creation_cleanup,
    );
}

/// A failed cleanup attempt scheduled a retry.
pub(crate) fn cleanup_retry(attempt: u32, delay_ms: u64, creation_cleanup: bool) {
    tracing::info!(
        target: "circus_sandbox",
        event = "sandbox.cleanup.retry",
        attempt = u64::from(attempt),
        delay_ms = delay_ms,
        creation_cleanup = creation_cleanup,
    );
}

/// Background cleanup after a failed creation confirmed completion.
pub(crate) fn creation_cleanup_completed(attempts: u32) {
    tracing::info!(
        target: "circus_sandbox",
        event = "sandbox.create.cleanup.completed",
        attempts = u64::from(attempts),
    );
}

/// One pool acquire call settled.
pub(crate) fn pool_acquire_completed(outcome: &'static str) {
    tracing::info!(
        target: "circus_sandbox",
        event = "pool.acquire.completed",
        outcome = outcome,
    );
}

/// One ready pool slot was handed to a caller.
pub(crate) fn pool_handoff() {
    tracing::info!(target: "circus_sandbox", event = "pool.lease.handed_off");
}

/// A lease was returned before its TTL.
pub(crate) fn pool_release() {
    tracing::info!(target: "circus_sandbox", event = "pool.lease.released");
}

/// A lease reached its TTL and was revoked.
pub(crate) fn pool_expired() {
    tracing::info!(target: "circus_sandbox", event = "pool.lease.expired");
}

/// A pool slot reached `Ready`.
pub(crate) fn pool_ready() {
    tracing::info!(target: "circus_sandbox", event = "pool.slot.ready");
}

/// A pool slot was retired for destruction.
pub(crate) fn pool_retired() {
    tracing::info!(target: "circus_sandbox", event = "pool.slot.retired");
}

/// A pool slot was recreated after initial warm-up.
pub(crate) fn pool_refilled() {
    tracing::info!(target: "circus_sandbox", event = "pool.slot.refilled");
}

/// The backend adapter selected for one creation.
pub(crate) fn backend_selected(backend: &'static str) {
    tracing::info!(target: "circus_sandbox", event = "backend.selected", backend);
}

/// Engine connection negotiated.
pub(crate) fn backend_connect_completed(socket: &'static str, outcome: &'static str) {
    tracing::info!(
        target: "circus_sandbox",
        event = "backend.connect.completed",
        socket = socket,
        outcome = outcome,
    );
}

/// Backend preparation (engine inspection and local image lookup) settled.
pub(crate) fn backend_prepare_completed(outcome: &'static str) {
    tracing::info!(
        target: "circus_sandbox",
        event = "backend.prepare.completed",
        outcome = outcome,
    );
}

/// Opens the `backend.create` span.
pub(crate) fn backend_create_span() -> tracing::Span {
    tracing::info_span!(target: "circus_sandbox", "backend.create", backend = "docker")
}

/// One engine create call settled.
pub(crate) fn backend_create_completed(
    outcome: &'static str,
    create_pending: bool,
    error_kind: &'static str,
) {
    tracing::info!(
        target: "circus_sandbox",
        event = "backend.create.completed",
        outcome = outcome,
        create_pending = create_pending,
        error_kind = error_kind,
    );
}

/// One engine recovery call settled.
pub(crate) fn backend_recovery_completed(ownership: &'static str) {
    tracing::info!(
        target: "circus_sandbox",
        event = "backend.recovery.completed",
        ownership = ownership,
    );
}

/// One backend start, pause, or resume call settled.
pub(crate) fn backend_transition_completed(
    operation: &'static str,
    outcome: &'static str,
    error_kind: &'static str,
) {
    tracing::info!(
        target: "circus_sandbox",
        event = "backend.transition.completed",
        operation = operation,
        outcome = outcome,
        error_kind = error_kind,
    );
}

/// One engine exec call settled.
pub(crate) fn backend_exec_completed(
    duration_ms: u64,
    exit_code: Option<i64>,
    stdout_bytes: usize,
    stderr_bytes: usize,
    stdin_kind: &'static str,
    outcome: &'static str,
) {
    tracing::info!(
        target: "circus_sandbox",
        event = "backend.exec.completed",
        duration_ms = duration_ms,
        exit_code = ?exit_code,
        stdout_bytes = count(stdout_bytes),
        stderr_bytes = count(stderr_bytes),
        stdin_kind = stdin_kind,
        outcome = outcome,
    );
}

fn count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
