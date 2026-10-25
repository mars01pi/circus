//! HTTP-layer tests over a real listener and a test-only in-process backend.
//!
//! The backend implements the crate's private `Backend` trait exactly as
//! `lifecycle/tests.rs` does. Handlers are not faked: every request goes
//! through the real router, auth middleware, DTO conversion, lifecycle worker,
//! and workspace implementation.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use reqwest::{Method, Response, StatusCode};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::api::Sandbox;
use crate::infrastructure::{
    Backend, Capabilities, CommonSpec, EngineError, EngineErrorKind, ExecDone, HostReport,
    InstanceId, OutputLimit as EngineOutputLimit, OutputPhase as EngineOutputPhase, PrepareError,
    ProcessSpec, Recovered, UploadEntry, WorkspaceEntry,
};

use super::HttpConfig;
use super::server::{HttpError, HttpServer, Served, ShutdownReport};
use super::state::{BoxSandboxFuture, SandboxFactory};

const TOKEN: &str = "test-token";
const ALPHA: &str = "tenant-alpha";
const BETA: &str = "tenant-beta";

#[derive(Clone, Default)]
struct FakeBackend {
    recoveries: Arc<Mutex<VecDeque<Recovered>>>,
    exec: Arc<Mutex<Option<Result<ExecDone, EngineError>>>>,
}

impl FakeBackend {
    fn new() -> Self {
        Self::default()
    }

    fn set_exec(&self, result: Result<ExecDone, EngineError>) {
        *self.exec.lock().expect("exec lock") = Some(result);
    }

    fn push_recovery(&self, outcome: Recovered) {
        self.recoveries
            .lock()
            .expect("recoveries lock")
            .push_back(outcome);
    }
}

#[allow(clippy::manual_async_fn)]
impl Backend for FakeBackend {
    fn kind(&self) -> &'static str {
        "test"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            pause_resume: true,
            file_transport: true,
            workspace_snapshot: true,
            spawn_process: true,
        }
    }

    fn prepare(&self) -> impl Future<Output = Result<(), PrepareError>> + Send {
        async { Ok(()) }
    }

    fn create_owned(
        &self,
        _name: &str,
        _spec: &CommonSpec,
        _nonce: &str,
    ) -> impl Future<Output = Result<InstanceId, EngineError>> + Send {
        async { Ok(InstanceId::new("fake-instance".to_string())) }
    }

    fn recover_owned(
        &self,
        _name: &str,
        _nonce: &str,
        _retained: Option<&InstanceId>,
    ) -> impl Future<Output = Recovered> + Send {
        let outcome = self
            .recoveries
            .lock()
            .expect("recoveries lock")
            .pop_front()
            .unwrap_or(Recovered::Removed);
        async move { outcome }
    }

    fn start(&self, _id: &InstanceId) -> impl Future<Output = Result<(), EngineError>> + Send {
        async { Ok(()) }
    }

    fn pause(&self, _id: &InstanceId) -> impl Future<Output = Result<(), EngineError>> + Send {
        async { Ok(()) }
    }

    fn resume(&self, _id: &InstanceId) -> impl Future<Output = Result<(), EngineError>> + Send {
        async { Ok(()) }
    }

    fn upload(
        &self,
        _id: &InstanceId,
        _user: &str,
        _workspace: &str,
        entry: UploadEntry,
        _max_bytes: u64,
    ) -> impl Future<Output = Result<(), EngineError>> + Send {
        async move {
            drop(entry);
            Ok(())
        }
    }

    fn download_file(
        &self,
        _id: &InstanceId,
        _user: &str,
        _workspace: &str,
        _relative: &str,
        _max_bytes: usize,
    ) -> impl Future<Output = Result<Vec<u8>, EngineError>> + Send {
        async { Err(unused("download_file")) }
    }

    fn remove_file(
        &self,
        _id: &InstanceId,
        _user: &str,
        _workspace: &str,
        _relative: &str,
    ) -> impl Future<Output = Result<(), EngineError>> + Send {
        async { Err(unused("remove_file")) }
    }

    fn exec(
        &self,
        _id: &InstanceId,
        _user: &str,
        _workdir: &str,
        _argv: &[String],
        _output_limit: usize,
    ) -> impl Future<Output = Result<ExecDone, EngineError>> + Send {
        let result = self.exec.lock().expect("exec lock").take();
        async move {
            result.unwrap_or(Ok(ExecDone {
                exit_code: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            }))
        }
    }

    fn spawn_process(
        &self,
        _id: &InstanceId,
        _spec: ProcessSpec<'_>,
    ) -> impl Future<Output = Result<crate::infrastructure::BackendProcess, EngineError>> + Send
    {
        async { Err(unused("spawn_process")) }
    }

    fn host_report(
        &self,
        _id: &InstanceId,
    ) -> impl Future<Output = Result<HostReport, EngineError>> + Send {
        async { Ok(HostReport::default()) }
    }

    fn list_workspace(
        &self,
        _id: &InstanceId,
        _user: &str,
        _workspace: &str,
        _max_bytes: u64,
        _max_entries: usize,
    ) -> impl Future<Output = Result<Vec<WorkspaceEntry>, EngineError>> + Send {
        async { Err(unused("list_workspace")) }
    }
}

fn unused(operation: &str) -> EngineError {
    EngineError {
        message: format!("unused fake operation: {operation}"),
        not_found: false,
        kind: EngineErrorKind::Other,
        create_pending: false,
        create_id: None,
        output: None,
        policy: None,
    }
}

struct FakeFactory {
    backend: FakeBackend,
}

impl SandboxFactory for FakeFactory {
    fn create(
        &self,
        _backend: crate::api::BackendConfig,
        config: crate::api::SandboxConfig,
    ) -> BoxSandboxFuture {
        let backend = self.backend.clone();
        Box::pin(async move { Sandbox::create_for_test(backend, config).await })
    }
}

struct TestServer {
    base: String,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<Served, HttpError>>,
}

impl TestServer {
    async fn start(config: HttpConfig, backend: FakeBackend) -> Self {
        let factory: Arc<dyn SandboxFactory> = Arc::new(FakeFactory { backend });
        let server = HttpServer::bind_with_factory(config, factory)
            .await
            .expect("bind");
        let base = format!("http://{}", server.local_addr());
        let (shutdown, receiver) = oneshot::channel();
        let handle = tokio::spawn(async move {
            server
                .serve_with_shutdown(async {
                    let _ = receiver.await;
                })
                .await
        });
        wait_ready(&base).await;
        Self {
            base,
            shutdown,
            handle,
        }
    }

    async fn stop(self) -> ShutdownReport {
        let _ = self.shutdown.send(());
        self.handle.await.expect("join").expect("serve").report
    }
}

async fn wait_ready(base: &str) {
    let client = client_for(base, TOKEN, ALPHA);
    for _ in 0..100 {
        match client.get("/healthz").await {
            response if response.status() == StatusCode::OK => return,
            _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }
    panic!("HTTP server did not become ready");
}

#[derive(Clone)]
struct Client {
    http: reqwest::Client,
    base: String,
    token: &'static str,
    tenant: &'static str,
}

fn client_for(base: &str, token: &'static str, tenant: &'static str) -> Client {
    Client {
        http: reqwest::Client::new(),
        base: base.to_string(),
        token,
        tenant,
    }
}

impl Client {
    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(self.token)
            .header("x-circus-tenant", self.tenant)
    }

    async fn get(&self, path: &str) -> Response {
        self.request(Method::GET, path).send().await.expect("get")
    }

    async fn create(&self, max_file_bytes: u64) -> String {
        let mut body = create_body();
        body["sandbox"]["max_file_bytes"] = json!(max_file_bytes);
        let response = self
            .request(Method::POST, "/v1/sandboxes")
            .json(&body)
            .send()
            .await
            .expect("create");
        assert_eq!(response.status(), StatusCode::CREATED);
        let value: Value = response.json().await.expect("create body");
        assert_eq!(value["phase"], "created");
        value["id"].as_str().expect("id").to_string()
    }
}

fn config() -> HttpConfig {
    HttpConfig::new(
        "127.0.0.1:0".parse::<SocketAddr>().expect("address"),
        TOKEN,
        [ALPHA, BETA],
    )
    .expect("config")
}

fn create_body() -> Value {
    json!({
        "backend": {
            "kind": "docker",
            "endpoint": "unix:///var/run/docker.sock",
            "image": "alpine:3.20",
            "runtime": "runc"
        },
        "sandbox": {
            "memory_bytes": 8 * 1024 * 1024,
            "cpu_millis": 1000,
            "pids": 8,
            "workspace_bytes": 1024 * 1024,
            "max_file_bytes": 4096,
            "isolation": {
                "workspace_path": "/workspace",
                "guest_uid": 1000,
                "guest_gid": 1000
            },
            "io": {
                "output_limit_bytes": 1024,
                "command_timeout_ms": 2000,
                "file_timeout_ms": 2000,
                "lifecycle_timeout_ms": 2000
            }
        }
    })
}

async fn expect_error(response: Response, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status);
    let body: Value = response.json().await.expect("error body");
    assert_eq!(body["code"], code);
    assert!(body["message"].is_string());
    assert_eq!(body.as_object().expect("object").len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_write_read_snapshot_restore_exec_and_delete() {
    let backend = FakeBackend::new();
    backend.set_exec(Ok(ExecDone {
        exit_code: 7,
        stdout: b"hello\n".to_vec(),
        stderr: b"warn\n".to_vec(),
    }));
    let server = TestServer::start(config(), backend).await;
    let alpha = client_for(&server.base, TOKEN, ALPHA);

    let id = alpha.create(4096).await;

    let status = alpha.get(&format!("/v1/sandboxes/{id}")).await;
    assert_eq!(status.status(), StatusCode::OK);
    let body: Value = status.json().await.expect("status");
    assert_eq!(body["id"], id);
    assert_eq!(body["phase"], "created");

    let write = alpha
        .request(Method::PUT, &format!("/v1/sandboxes/{id}/files/a.txt"))
        .body("hello")
        .send()
        .await
        .expect("write");
    assert_eq!(write.status(), StatusCode::NO_CONTENT);

    let read = alpha.get(&format!("/v1/sandboxes/{id}/files/a.txt")).await;
    assert_eq!(read.status(), StatusCode::OK);
    assert_eq!(read.bytes().await.expect("bytes").as_ref(), b"hello");

    let captured = alpha
        .request(Method::POST, &format!("/v1/sandboxes/{id}/snapshots"))
        .json(&json!({
            "max_total_bytes": 1024,
            "max_entries": 16,
            "max_archive_bytes": 2048
        }))
        .send()
        .await
        .expect("capture");
    assert_eq!(captured.status(), StatusCode::CREATED);
    let snapshot: Value = captured.json().await.expect("snapshot");
    let snapshot_id = snapshot["snapshot_id"].as_str().expect("snapshot id");
    assert_eq!(snapshot["total_bytes"], 5);
    assert!(snapshot["entries"].as_u64().expect("entries") >= 1);

    let second = alpha.create(4096).await;
    let restored = alpha
        .request(
            Method::POST,
            &format!("/v1/sandboxes/{second}/snapshots/{snapshot_id}/restore"),
        )
        .send()
        .await
        .expect("restore");
    assert_eq!(restored.status(), StatusCode::NO_CONTENT);
    let copied = alpha
        .get(&format!("/v1/sandboxes/{second}/files/a.txt"))
        .await;
    assert_eq!(copied.bytes().await.expect("copied").as_ref(), b"hello");

    let started = alpha
        .request(Method::POST, &format!("/v1/sandboxes/{id}/start"))
        .send()
        .await
        .expect("start");
    assert_eq!(started.status(), StatusCode::NO_CONTENT);

    let exec = alpha
        .request(Method::POST, &format!("/v1/sandboxes/{id}/exec"))
        .json(&json!({
            "argv": ["sh", "-c", "echo hello"],
            "timeout_ms": 1000,
            "output_limit_bytes": 512
        }))
        .send()
        .await
        .expect("exec");
    assert_eq!(exec.status(), StatusCode::OK);
    let output: Value = exec.json().await.expect("exec body");
    assert_eq!(output["exit_code"], 7);
    assert_eq!(output["stdout_base64"], "aGVsbG8K");
    assert_eq!(output["stderr_base64"], "d2Fybgo=");

    let deleted = alpha
        .request(Method::DELETE, &format!("/v1/sandboxes/{id}"))
        .send()
        .await
        .expect("delete");
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let gone = alpha.get(&format!("/v1/sandboxes/{id}")).await;
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);

    let report = server.stop().await;
    assert_eq!(report.sandboxes, 1, "only the second sandbox remains");
    assert!(report.complete());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_rejects_output_above_the_configured_limit() {
    let server = TestServer::start(config(), FakeBackend::new()).await;
    let alpha = client_for(&server.base, TOKEN, ALPHA);
    let id = alpha.create(4096).await;
    let _ = alpha
        .request(Method::POST, &format!("/v1/sandboxes/{id}/start"))
        .send()
        .await
        .expect("start");

    let exec = alpha
        .request(Method::POST, &format!("/v1/sandboxes/{id}/exec"))
        .json(&json!({
            "argv": ["true"],
            "timeout_ms": 1000,
            "output_limit_bytes": 2048
        }))
        .send()
        .await
        .expect("exec");
    expect_error(exec, StatusCode::PAYLOAD_TOO_LARGE, "limit_exceeded").await;

    let status: Value = alpha
        .get(&format!("/v1/sandboxes/{id}"))
        .await
        .json()
        .await
        .expect("status");
    assert_eq!(status["phase"], "running");
    let _ = server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_engine_limit_destroys_the_instance() {
    let backend = FakeBackend::new();
    backend.set_exec(Err(EngineError {
        message: "output limit".to_string(),
        not_found: false,
        kind: EngineErrorKind::Limit,
        create_pending: false,
        create_id: None,
        output: Some(EngineOutputLimit {
            limit: 512,
            phase: EngineOutputPhase::Stdout,
        }),
        policy: None,
    }));
    let server = TestServer::start(config(), backend).await;
    let alpha = client_for(&server.base, TOKEN, ALPHA);
    let id = alpha.create(4096).await;
    let _ = alpha
        .request(Method::POST, &format!("/v1/sandboxes/{id}/start"))
        .send()
        .await
        .expect("start");

    let exec = alpha
        .request(Method::POST, &format!("/v1/sandboxes/{id}/exec"))
        .json(&json!({
            "argv": ["yes"],
            "timeout_ms": 1000,
            "output_limit_bytes": 512
        }))
        .send()
        .await
        .expect("exec");
    expect_error(exec, StatusCode::PAYLOAD_TOO_LARGE, "limit_exceeded").await;

    let status: Value = alpha
        .get(&format!("/v1/sandboxes/{id}"))
        .await
        .json()
        .await
        .expect("status");
    assert_eq!(status["phase"], "destroyed");
    let _ = server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_limits_are_enforced_and_reported_as_limit_exceeded() {
    let server = TestServer::start(config(), FakeBackend::new()).await;
    let alpha = client_for(&server.base, TOKEN, ALPHA);
    let id = alpha.create(4).await;

    let too_large = alpha
        .request(Method::PUT, &format!("/v1/sandboxes/{id}/files/big.txt"))
        .body("12345")
        .send()
        .await
        .expect("write");
    expect_error(too_large, StatusCode::PAYLOAD_TOO_LARGE, "limit_exceeded").await;

    let exact = alpha
        .request(Method::PUT, &format!("/v1/sandboxes/{id}/files/exact.txt"))
        .body("1234")
        .send()
        .await
        .expect("write");
    assert_eq!(exact.status(), StatusCode::NO_CONTENT);

    let body_limit_config = config()
        .with_max_body_bytes(1024)
        .expect("body limit config");
    let small_server = TestServer::start(body_limit_config, FakeBackend::new()).await;
    let small = client_for(&small_server.base, TOKEN, ALPHA);
    let id = small.create(4096).await;
    let oversized = small
        .request(Method::PUT, &format!("/v1/sandboxes/{id}/files/toobig.txt"))
        .body(vec![b'x'; 2048])
        .send()
        .await
        .expect("write");
    expect_error(oversized, StatusCode::PAYLOAD_TOO_LARGE, "limit_exceeded").await;

    let _ = server.stop().await;
    let _ = small_server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_tenant_cannot_observe_or_destroy_a_sandbox() {
    let server = TestServer::start(config(), FakeBackend::new()).await;
    let alpha = client_for(&server.base, TOKEN, ALPHA);
    let beta = client_for(&server.base, TOKEN, BETA);
    let id = alpha.create(4096).await;
    let _ = alpha
        .request(Method::PUT, &format!("/v1/sandboxes/{id}/files/secret.txt"))
        .body("alpha only")
        .send()
        .await
        .expect("write");
    let snapshot: Value = alpha
        .request(Method::POST, &format!("/v1/sandboxes/{id}/snapshots"))
        .json(&json!({
            "max_total_bytes": 1024,
            "max_entries": 16,
            "max_archive_bytes": 1024
        }))
        .send()
        .await
        .expect("capture")
        .json()
        .await
        .expect("snapshot");
    let snapshot_id = snapshot["snapshot_id"].as_str().expect("id");

    for path in [
        format!("/v1/sandboxes/{id}"),
        format!("/v1/sandboxes/{id}/files/secret.txt"),
    ] {
        let response = beta.get(&path).await;
        expect_error(response, StatusCode::NOT_FOUND, "not_found").await;
    }
    for path in [
        format!("/v1/sandboxes/{id}/start"),
        format!("/v1/sandboxes/{id}/pause"),
        format!("/v1/sandboxes/{id}/resume"),
        format!("/v1/sandboxes/{id}/snapshots/{snapshot_id}/restore"),
    ] {
        let response = beta
            .request(Method::POST, &path)
            .json(&json!({}))
            .send()
            .await
            .expect("post");
        expect_error(response, StatusCode::NOT_FOUND, "not_found").await;
    }
    let delete = beta
        .request(Method::DELETE, &format!("/v1/sandboxes/{id}"))
        .send()
        .await
        .expect("delete");
    expect_error(delete, StatusCode::NOT_FOUND, "not_found").await;

    assert_eq!(
        alpha.get(&format!("/v1/sandboxes/{id}")).await.status(),
        StatusCode::OK
    );
    let _ = server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_cleanup_keeps_the_handle_for_retry() {
    let backend = FakeBackend::new();
    backend.push_recovery(Recovered::StillPresent {
        detail: "engine still reports the container".to_string(),
    });
    let server = TestServer::start(config(), backend.clone()).await;
    let alpha = client_for(&server.base, TOKEN, ALPHA);
    let id = alpha.create(4096).await;

    let failed = alpha
        .request(Method::DELETE, &format!("/v1/sandboxes/{id}"))
        .send()
        .await
        .expect("delete");
    expect_error(
        failed,
        StatusCode::INTERNAL_SERVER_ERROR,
        "cleanup_required",
    )
    .await;

    let retained: Value = alpha
        .get(&format!("/v1/sandboxes/{id}"))
        .await
        .json()
        .await
        .expect("status");
    assert_eq!(retained["phase"], "cleanup_required");

    backend.push_recovery(Recovered::Removed);
    let retried = alpha
        .request(Method::DELETE, &format!("/v1/sandboxes/{id}"))
        .send()
        .await
        .expect("retry");
    assert_eq!(retried.status(), StatusCode::NO_CONTENT);
    let report = server.stop().await;
    assert!(report.complete());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_service_limits_bound_capture_and_retention() {
    let config = config()
        .with_snapshot_limits(512, 8)
        .expect("snapshot limits")
        .with_max_snapshots_per_tenant(1)
        .expect("snapshot count");
    let server = TestServer::start(config, FakeBackend::new()).await;
    let alpha = client_for(&server.base, TOKEN, ALPHA);
    let id = alpha.create(4096).await;

    let over_budget = alpha
        .request(Method::POST, &format!("/v1/sandboxes/{id}/snapshots"))
        .json(&json!({
            "max_total_bytes": 1024,
            "max_entries": 8,
            "max_archive_bytes": 1024
        }))
        .send()
        .await
        .expect("capture");
    expect_error(over_budget, StatusCode::PAYLOAD_TOO_LARGE, "limit_exceeded").await;

    let body = json!({
        "max_total_bytes": 256,
        "max_entries": 8,
        "max_archive_bytes": 256
    });
    let first = alpha
        .request(Method::POST, &format!("/v1/sandboxes/{id}/snapshots"))
        .json(&body)
        .send()
        .await
        .expect("capture");
    assert_eq!(first.status(), StatusCode::CREATED);

    let second = alpha
        .request(Method::POST, &format!("/v1/sandboxes/{id}/snapshots"))
        .json(&body)
        .send()
        .await
        .expect("capture");
    expect_error(second, StatusCode::PAYLOAD_TOO_LARGE, "limit_exceeded").await;

    let report = server.stop().await;
    assert_eq!(report.snapshots, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_configuration_and_unsupported_backend_are_distinct() {
    let server = TestServer::start(config(), FakeBackend::new()).await;
    let alpha = client_for(&server.base, TOKEN, ALPHA);

    let mut zero_memory = create_body();
    zero_memory["sandbox"]["memory_bytes"] = json!(0);
    let response = alpha
        .request(Method::POST, "/v1/sandboxes")
        .json(&zero_memory)
        .send()
        .await
        .expect("create");
    expect_error(response, StatusCode::BAD_REQUEST, "invalid_request").await;

    let mut unknown_field = create_body();
    unknown_field["sandbox"]["surprise"] = json!(true);
    let response = alpha
        .request(Method::POST, "/v1/sandboxes")
        .json(&unknown_field)
        .send()
        .await
        .expect("create");
    expect_error(response, StatusCode::BAD_REQUEST, "invalid_request").await;

    let mut unknown_backend = create_body();
    unknown_backend["backend"]["kind"] = json!("kubernetes");
    let response = alpha
        .request(Method::POST, "/v1/sandboxes")
        .json(&unknown_backend)
        .send()
        .await
        .expect("create");
    expect_error(response, StatusCode::NOT_IMPLEMENTED, "unsupported_backend").await;

    let mut unsupported_runtime = create_body();
    unsupported_runtime["backend"]["runtime"] = json!("kata");
    let response = alpha
        .request(Method::POST, "/v1/sandboxes")
        .json(&unsupported_runtime)
        .send()
        .await
        .expect("create");
    expect_error(response, StatusCode::NOT_IMPLEMENTED, "unsupported_backend").await;

    let response = alpha
        .request(Method::POST, "/v1/sandboxes")
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await
        .expect("create");
    expect_error(response, StatusCode::BAD_REQUEST, "invalid_request").await;

    let _ = server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_destroys_every_retained_sandbox() {
    let server = TestServer::start(config(), FakeBackend::new()).await;
    let alpha = client_for(&server.base, TOKEN, ALPHA);
    let _first = alpha.create(4096).await;
    let _second = alpha.create(4096).await;
    let report = server.stop().await;
    assert_eq!(report.sandboxes, 2);
    assert!(report.complete());
    assert!(report.failures.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_reports_unconfirmed_cleanup() {
    let backend = FakeBackend::new();
    backend.push_recovery(Recovered::StillPresent {
        detail: "still present".to_string(),
    });
    let server = TestServer::start(config(), backend).await;
    let alpha = client_for(&server.base, TOKEN, ALPHA);
    let id = alpha.create(4096).await;
    let report = server.stop().await;
    assert_eq!(report.sandboxes, 1);
    assert!(!report.complete());
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].sandbox_id, id);
    assert_eq!(report.failures[0].code, "cleanup_required");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openapi_matches_the_registered_router() {
    const OPENAPI: &str = include_str!("../../docs/openapi.json");
    let document: Value = serde_json::from_str(OPENAPI).expect("openapi.json parses");

    let mut documented: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (path, item) in document["paths"].as_object().expect("paths object") {
        let methods = documented.entry(path.clone()).or_default();
        for method in item.as_object().expect("path item").keys() {
            if matches!(
                method.as_str(),
                "get" | "put" | "post" | "delete" | "patch" | "options" | "head" | "trace"
            ) {
                methods.insert(method.clone());
            }
        }
    }

    let mut registered: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for spec in super::routes::ROUTES {
        registered
            .entry(spec.openapi_path.to_string())
            .or_default()
            .insert(spec.method.as_str().to_ascii_lowercase());
    }
    assert_eq!(documented, registered, "OpenAPI and route table drifted");

    let server = TestServer::start(config(), FakeBackend::new()).await;
    let alpha = client_for(&server.base, TOKEN, ALPHA);
    for (path, methods) in &registered {
        let concrete = path
            .replace("{id}", "00000000-0000-4000-8000-000000000000")
            .replace("{snapshot_id}", "11111111-1111-4111-8111-111111111111")
            .replace("{path}", "a.txt");
        let probe = Method::from_bytes(b"PROBE").expect("probe method");
        let response = alpha.request(probe, &concrete).send().await.expect("probe");
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "documented path {path} is missing from the router"
        );
        for method in methods {
            let method = Method::from_bytes(method.to_ascii_uppercase().as_bytes())
                .expect("documented method");
            let request = alpha.request(method.clone(), &concrete);
            let response = if method == Method::GET || method == Method::DELETE {
                request.send().await.expect("documented call")
            } else {
                request
                    .header("content-type", "application/json")
                    .body("{}")
                    .send()
                    .await
                    .expect("documented call")
            };
            assert_ne!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "documented {method} {path} is not registered"
            );
        }
    }
    let _ = server.stop().await;
}
