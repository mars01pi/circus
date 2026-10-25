//! Public-surface HTTP tests over a real listener.
//!
//! These exercise authentication, tenant authorization, routing, and error
//! bodies without requiring Docker. Success paths that need a backend live in
//! `src/http/tests.rs` (in-process backend) and in the ignored Docker test.

#![cfg(feature = "http")]

use std::net::SocketAddr;

use circus_sandbox::http::{HttpConfig, HttpServer, Served, ShutdownReport};
use reqwest::{Method, Response, StatusCode};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const TOKEN: &str = "integration-token";
const TENANT: &str = "tenant-a";

struct TestServer {
    base: String,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<Served, circus_sandbox::http::HttpError>>,
}

impl TestServer {
    async fn start(config: HttpConfig) -> Self {
        let server = HttpServer::bind(config).await.expect("bind");
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
    let client = Client::new(base);
    for _ in 0..100 {
        match client.request(Method::GET, "/healthz").send().await {
            Ok(response) if response.status() == StatusCode::OK => return,
            _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }
    panic!("HTTP server did not become ready");
}

struct Client {
    http: reqwest::Client,
    base: String,
    token: Option<&'static str>,
    tenant: Option<&'static str>,
}

impl Client {
    fn new(base: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.to_string(),
            token: Some(TOKEN),
            tenant: Some(TENANT),
        }
    }

    fn without_token(mut self) -> Self {
        self.token = None;
        self
    }

    fn with_token(mut self, token: &'static str) -> Self {
        self.token = Some(token);
        self
    }

    fn with_tenant(mut self, tenant: &'static str) -> Self {
        self.tenant = Some(tenant);
        self
    }

    fn without_tenant(mut self) -> Self {
        self.tenant = None;
        self
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if let Some(token) = self.token {
            request = request.bearer_auth(token);
        }
        if let Some(tenant) = self.tenant {
            request = request.header("x-circus-tenant", tenant);
        }
        request
    }
}

fn config() -> HttpConfig {
    HttpConfig::new(
        "127.0.0.1:0".parse::<SocketAddr>().expect("address"),
        TOKEN,
        ["tenant-a", "tenant-b"],
    )
    .expect("config")
}

async fn expect_error(response: Response, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status);
    let body: Value = response.json().await.expect("error body");
    assert_eq!(body["code"], code);
    assert!(body["message"].is_string());
    assert_eq!(body.as_object().expect("object").len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_and_wrong_tokens_are_unauthorized() {
    let server = TestServer::start(config()).await;
    let no_token = Client::new(&server.base).without_token();
    expect_error(
        no_token
            .request(Method::GET, "/v1/sandboxes/absent")
            .send()
            .await
            .expect("request"),
        StatusCode::UNAUTHORIZED,
        "unauthorized",
    )
    .await;
    let wrong = Client::new(&server.base).with_token("wrong-token");
    expect_error(
        wrong
            .request(Method::GET, "/healthz")
            .send()
            .await
            .expect("request"),
        StatusCode::UNAUTHORIZED,
        "unauthorized",
    )
    .await;
    let _ = server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authentication_precedes_route_and_tenant_evaluation() {
    let server = TestServer::start(config()).await;
    let no_token = Client::new(&server.base).without_token();
    expect_error(
        no_token
            .request(Method::GET, "/not-a-route")
            .send()
            .await
            .expect("request"),
        StatusCode::UNAUTHORIZED,
        "unauthorized",
    )
    .await;
    let no_tenant = Client::new(&server.base).without_tenant();
    expect_error(
        no_tenant
            .request(Method::GET, "/healthz")
            .send()
            .await
            .expect("request"),
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    let wrong_tenant = Client::new(&server.base).with_tenant("tenant-c");
    expect_error(
        wrong_tenant
            .request(Method::GET, "/healthz")
            .send()
            .await
            .expect("request"),
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    let _ = server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthz_reports_only_process_liveness() {
    let server = TestServer::start(config()).await;
    let client = Client::new(&server.base);
    let response = client
        .request(Method::GET, "/healthz")
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("body");
    assert_eq!(body, json!({ "status": "ok" }));
    let _ = server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_json_and_unknown_routes_have_stable_errors() {
    let server = TestServer::start(config()).await;
    let client = Client::new(&server.base);
    expect_error(
        client
            .request(Method::POST, "/v1/sandboxes")
            .header("content-type", "application/json")
            .body("{")
            .send()
            .await
            .expect("request"),
        StatusCode::BAD_REQUEST,
        "invalid_request",
    )
    .await;
    expect_error(
        client
            .request(Method::GET, "/not-a-route")
            .send()
            .await
            .expect("request"),
        StatusCode::NOT_FOUND,
        "not_found",
    )
    .await;
    expect_error(
        client
            .request(Method::DELETE, "/healthz")
            .send()
            .await
            .expect("request"),
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
    )
    .await;
    let _ = server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_backend_is_rejected_with_501() {
    let server = TestServer::start(config()).await;
    let client = Client::new(&server.base);
    let mut body = create_body();
    body["backend"]["kind"] = json!("containerd");
    let response = client
        .request(Method::POST, "/v1/sandboxes")
        .json(&body)
        .send()
        .await
        .expect("request");
    expect_error(response, StatusCode::NOT_IMPLEMENTED, "unsupported_backend").await;
    let _ = server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_sandbox_operations_are_not_found() {
    let server = TestServer::start(config()).await;
    let client = Client::new(&server.base);
    let id = "00000000-0000-4000-8000-000000000000";
    for (method, path) in [
        (Method::GET, format!("/v1/sandboxes/{id}")),
        (Method::DELETE, format!("/v1/sandboxes/{id}")),
        (Method::POST, format!("/v1/sandboxes/{id}/start")),
        (Method::POST, format!("/v1/sandboxes/{id}/pause")),
        (Method::POST, format!("/v1/sandboxes/{id}/resume")),
        (Method::POST, format!("/v1/sandboxes/{id}/exec")),
        (Method::GET, format!("/v1/sandboxes/{id}/files/a.txt")),
        (Method::PUT, format!("/v1/sandboxes/{id}/files/a.txt")),
        (Method::POST, format!("/v1/sandboxes/{id}/snapshots")),
        (
            Method::POST,
            format!("/v1/sandboxes/{id}/snapshots/00000000-0000-4000-8000-000000000001/restore"),
        ),
    ] {
        let response = client
            .request(method.clone(), &path)
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("request");
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} {path} must be tenant-scoped 404"
        );
        let body: Value = response.json().await.expect("error body");
        assert_eq!(body["code"], "not_found");
    }
    let _ = server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_limit_is_reported_as_limit_exceeded() {
    let config = config().with_max_body_bytes(128).expect("limit");
    let server = TestServer::start(config).await;
    let client = Client::new(&server.base);
    let response = client
        .request(Method::POST, "/v1/sandboxes")
        .json(&create_body())
        .send()
        .await
        .expect("request");
    expect_error(response, StatusCode::PAYLOAD_TOO_LARGE, "limit_exceeded").await;
    let _ = server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graceful_shutdown_without_sandboxes_is_complete() {
    let server = TestServer::start(config()).await;
    let report = server.stop().await;
    assert_eq!(report.sandboxes, 0);
    assert!(report.complete());
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
            "memory_bytes": 8388608,
            "cpu_millis": 1000,
            "pids": 8,
            "workspace_bytes": 1048576,
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
