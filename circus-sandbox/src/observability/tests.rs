//! Instrumentation tests. No Docker Engine or global subscriber is involved.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

use super::events::{self, CommandFacts};
use crate::{
    BackendConfig, IoLimits, IsolationConfig, ResourceLimits, Sandbox, SandboxConfig, SandboxError,
    SandboxMetrics,
};

#[derive(Debug, Clone)]
struct Captured {
    name: String,
    target: String,
    fields: BTreeMap<String, String>,
}

#[derive(Clone, Default)]
struct Capture {
    events: Arc<Mutex<Vec<Captured>>>,
    spans: Arc<Mutex<Vec<Captured>>>,
    next: Arc<AtomicU64>,
}

impl Capture {
    fn event(&self, name: &str) -> Captured {
        self.events
            .lock()
            .unwrap()
            .iter()
            .find(|captured| captured.fields.get("event").map(String::as_str) == Some(name))
            .cloned()
            .unwrap_or_else(|| panic!("event {name} was not captured"))
    }

    fn span(&self, name: &str) -> Captured {
        self.spans
            .lock()
            .unwrap()
            .iter()
            .find(|captured| captured.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("span {name} was not captured"))
    }

    fn text(&self) -> String {
        let events = self.events.lock().unwrap().clone();
        let spans = self.spans.lock().unwrap().clone();
        let mut text = String::new();
        for captured in events.into_iter().chain(spans) {
            text.push_str(&captured.name);
            text.push(' ');
            text.push_str(&captured.target);
            for (key, value) in captured.fields {
                text.push(' ');
                text.push_str(&key);
                text.push('=');
                text.push_str(&value);
            }
            text.push('\n');
        }
        text
    }
}

struct Fields<'a>(&'a mut BTreeMap<String, String>);

impl Visit for Fields<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attributes: &Attributes<'_>) -> Id {
        let mut fields = BTreeMap::new();
        attributes.record(&mut Fields(&mut fields));
        self.spans.lock().unwrap().push(Captured {
            name: attributes.metadata().name().to_string(),
            target: attributes.metadata().target().to_string(),
            fields,
        });
        Id::from_u64(self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = BTreeMap::new();
        event.record(&mut Fields(&mut fields));
        self.events.lock().unwrap().push(Captured {
            name: event.metadata().name().to_string(),
            target: event.metadata().target().to_string(),
            fields,
        });
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

fn capture<T>(run: impl FnOnce() -> T) -> (Capture, T) {
    let capture = Capture::default();
    let value = tracing::subscriber::with_default(capture.clone(), || {
        // Callsite interest is cached process-wide; a callsite first seen with
        // no subscriber would otherwise stay disabled for this scoped capture.
        tracing::callsite::rebuild_interest_cache();
        run()
    });
    (capture, value)
}

fn invalid_backend() -> BackendConfig {
    BackendConfig::docker("tcp://127.0.0.1:2375", "python:3.12-slim", "runc")
}

fn config() -> SandboxConfig {
    let limits = ResourceLimits::new(8 * 1024 * 1024, 100, 8, 1024 * 1024, 4096).unwrap();
    let isolation =
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 65534, 65534)
            .unwrap();
    SandboxConfig::new(
        limits,
        isolation,
        IoLimits::new(64 * 1024, Duration::from_secs(1)).unwrap(),
    )
    .unwrap()
}

#[test]
fn process_events_never_contain_arguments_or_environment_values() {
    let argv = vec![
        "/host/secret/dir/run".to_string(),
        "SENTINEL_ARG_VALUE".to_string(),
        "--token=SENTINEL_TOKEN".to_string(),
    ];
    let env = BTreeMap::from([
        ("API_KEY".to_string(), "SENTINEL_ENV_VALUE".to_string()),
        (
            "TOKEN_NAME".to_string(),
            "SENTINEL_SECOND_VALUE".to_string(),
        ),
    ]);
    let (capture, facts) = capture(|| {
        let facts = events::process_facts(&argv, &env);
        events::process_opened(&facts, 5_000, 16 * 1024);
        events::process_closed("exited", Some(0), None);
        events::exec_completed(&facts, 12, "ok", Some(0), 8, 0, false, None);
        facts
    });
    assert_eq!(
        facts,
        CommandFacts {
            program: "run".to_string(),
            argc: 3,
            env_overrides: 2,
        },
    );
    let opened = capture.event("sandbox.process.opened");
    assert_eq!(
        opened.fields.get("program").map(String::as_str),
        Some("run")
    );
    assert_eq!(opened.fields.get("argc").map(String::as_str), Some("3"));
    assert_eq!(
        opened.fields.get("env_overrides").map(String::as_str),
        Some("2"),
    );
    assert_eq!(
        opened.fields.get("timeout_ms").map(String::as_str),
        Some("5000"),
    );
    let closed = capture.event("sandbox.process.closed");
    assert_eq!(
        closed.fields.get("reason").map(String::as_str),
        Some("exited"),
    );
    let exec = capture.event("sandbox.exec.completed");
    assert_eq!(
        exec.fields.get("duration_ms").map(String::as_str),
        Some("12"),
    );
    assert_eq!(
        exec.fields.get("exit_code").map(String::as_str),
        Some("Some(0)"),
    );
    assert_eq!(
        exec.fields.get("output_limited").map(String::as_str),
        Some("false"),
    );
    let text = capture.text();
    for sentinel in [
        "SENTINEL_ARG_VALUE",
        "SENTINEL_TOKEN",
        "SENTINEL_ENV_VALUE",
        "SENTINEL_SECOND_VALUE",
        "API_KEY",
        "TOKEN_NAME",
        "/host/secret/dir",
    ] {
        assert!(!text.contains(sentinel), "leaked {sentinel} in {text}");
    }
    assert!(text.contains("program=run"));
}

#[test]
fn capability_event_names_the_missing_capability_without_details() {
    let (capture, ()) = capture(|| {
        events::capability_unavailable("pause and resume");
    });
    let event = capture.event("sandbox.capability.unavailable");
    assert_eq!(
        event.fields.get("capability").map(String::as_str),
        Some("pause and resume"),
    );
}

#[test]
fn create_emits_stable_span_and_outcome_without_docker() {
    let (capture, ()) = capture(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let metrics = SandboxMetrics::new();
            let result = Sandbox::create_with_metrics(invalid_backend(), config(), metrics).await;
            assert!(matches!(result, Err(SandboxError::BackendConfig(_))));
        });
    });
    let span = capture.span("sandbox.create");
    assert_eq!(
        span.fields.get("backend").map(String::as_str),
        Some("docker"),
    );
    let started = capture.event("sandbox.create.started");
    assert_eq!(
        started.fields.get("backend").map(String::as_str),
        Some("docker"),
    );
    let completed = capture.event("sandbox.create.completed");
    assert_eq!(
        completed.fields.get("outcome").map(String::as_str),
        Some("error"),
    );
    assert_eq!(
        completed.fields.get("cleanup_pending").map(String::as_str),
        Some("false"),
    );
    assert_eq!(
        completed.fields.get("error_kind").map(String::as_str),
        Some("backend_config"),
    );
    assert!(!capture.text().contains("tcp://127.0.0.1:2375"));
}

#[test]
fn cleanup_events_distinguish_ownership_and_pending_cleanup() {
    let (capture, ()) = capture(|| {
        events::cleanup_attempted("still_present", false, true);
        events::cleanup_retry(2, 200, true);
        events::creation_cleanup_completed(2);
    });
    let attempted = capture.event("sandbox.cleanup.attempted");
    assert_eq!(
        attempted.fields.get("ownership").map(String::as_str),
        Some("still_present"),
    );
    assert_eq!(
        attempted.fields.get("complete").map(String::as_str),
        Some("false"),
    );
    assert_eq!(
        attempted.fields.get("creation_cleanup").map(String::as_str),
        Some("true"),
    );
    let retry = capture.event("sandbox.cleanup.retry");
    assert_eq!(retry.fields.get("attempt").map(String::as_str), Some("2"));
    assert_eq!(
        retry.fields.get("delay_ms").map(String::as_str),
        Some("200"),
    );
    assert_eq!(
        capture
            .event("sandbox.create.cleanup.completed")
            .fields
            .get("attempts")
            .map(String::as_str),
        Some("2"),
    );
    assert!(!capture.text().contains("still present"));
}

#[test]
fn command_facts_sanitize_empty_and_non_graphic_programs() {
    assert_eq!(
        events::command_facts(&[], 0),
        CommandFacts {
            program: "program".to_string(),
            argc: 0,
            env_overrides: 0,
        }
    );
    assert_eq!(
        events::command_facts(&[String::from("   "), String::from("a")], 1).program,
        "program"
    );
    assert_eq!(
        events::command_facts(&[String::from("/tmp/bin/tool=evil"), String::from("x")], 0).program,
        "toolevil"
    );
}

#[test]
fn pool_and_backend_helper_events_use_fixed_names() {
    let (capture, ()) = capture(|| {
        let _create = events::create_span("docker");
        events::transition_completed("pause", "ok", false);
        events::snapshot_capture_completed("ok", false);
        events::snapshot_restore_completed("error", true);
        events::pool_acquire_completed("ok");
        events::pool_handoff();
        events::pool_release();
        events::pool_expired();
        events::pool_ready();
        events::pool_retired();
        events::pool_refilled();
        events::backend_selected("docker");
        events::backend_connect_completed("unix", "ok");
        events::backend_prepare_completed("ok");
        let _backend_create = events::backend_create_span();
        events::backend_create_completed("ok", false, "none");
        events::backend_recovery_completed("confirmed_absent");
        events::backend_transition_completed("start", "ok", "none");
        events::backend_exec_completed(3, Some(0), 1, 0, "none", "ok");
    });
    for name in [
        "sandbox.transition.completed",
        "sandbox.snapshot.capture.completed",
        "sandbox.snapshot.restore.completed",
        "pool.acquire.completed",
        "pool.lease.handed_off",
        "pool.lease.released",
        "pool.lease.expired",
        "pool.slot.ready",
        "pool.slot.retired",
        "pool.slot.refilled",
        "backend.selected",
        "backend.connect.completed",
        "backend.prepare.completed",
        "backend.create.completed",
        "backend.recovery.completed",
        "backend.transition.completed",
        "backend.exec.completed",
    ] {
        let _ = capture.event(name);
    }
    assert_eq!(
        capture
            .span("sandbox.create")
            .fields
            .get("backend")
            .map(String::as_str),
        Some("docker"),
    );
    assert_eq!(
        capture
            .event("backend.exec.completed")
            .fields
            .get("duration_ms")
            .map(String::as_str),
        Some("3"),
    );
}
