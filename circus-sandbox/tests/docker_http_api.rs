//! Opt-in full HTTP success paths against a real local Docker Engine.
//!
//! Ignored by default. Set `CIRCUS_DOCKER_SANDBOX=1` plus the endpoint and image
//! variables used by the other Docker tests, then run:
//!
//! ```bash
//! CIRCUS_DOCKER_SANDBOX=1 CIRCUS_DOCKER_ENDPOINT=unix:///var/run/docker.sock \
//!   cargo test --locked --test docker_http_api -- --ignored
//! ```

#![cfg(feature = "http")]

use std::net::SocketAddr;

use circus_sandbox::http::{HttpConfig, HttpError, HttpServer, Served};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const TOKEN: &str = "docker-http-token";
const TENANT: &str = "docker-tenant";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a local Docker Engine, runc, and an already-present image"]
async fn full_http_flow_over_a_real_container() {
    assert_eq!(
        std::env::var("CIRCUS_DOCKER_SANDBOX").as_deref(),
        Ok("1"),
        "set CIRCUS_DOCKER_SANDBOX=1 to opt into the real backend test"
    );
    let endpoint = std::env::var("CIRCUS_DOCKER_ENDPOINT").expect("CIRCUS_DOCKER_ENDPOINT");
    let endpoint = if endpoint.starts_with('/') {
        format!("unix://{endpoint}")
    } else {
        endpoint
    };
    let image = std::env::var("CIRCUS_DOCKER_IMAGE").unwrap_or_else(|_| "python:3.12-slim".into());

    let config = HttpConfig::new(
        "127.0.0.1:0".parse::<SocketAddr>().expect("address"),
        TOKEN,
        [TENANT],
    )
    .expect("config");
    let server = HttpServer::bind(config).await.expect("bind");
    let base = format!("http://{}", server.local_addr());
    let (shutdown, receiver) = oneshot::channel();
    let handle: JoinHandle<Result<Served, HttpError>> = tokio::spawn(async move {
        server
            .serve_with_shutdown(async {
                let _ = receiver.await;
            })
            .await
    });
    let client = reqwest::Client::new();
    let request = |method: Method, path: &str| {
        client
            .request(method, format!("{base}{path}"))
            .bearer_auth(TOKEN)
            .header("x-circus-tenant", TENANT)
    };

    let created = request(Method::POST, "/v1/sandboxes")
        .json(&json!({
            "backend": {
                "kind": "docker",
                "endpoint": endpoint,
                "image": image,
                "runtime": "runc"
            },
            "sandbox": {
                "memory_bytes": 67108864,
                "cpu_millis": 1000,
                "pids": 32,
                "workspace_bytes": 8388608,
                "max_file_bytes": 65536,
                "isolation": {
                    "workspace_path": "/workspace",
                    "guest_uid": 65534,
                    "guest_gid": 65534
                },
                "io": {
                    "output_limit_bytes": 65536,
                    "command_timeout_ms": 5000,
                    "file_timeout_ms": 5000,
                    "lifecycle_timeout_ms": 5000
                }
            }
        }))
        .send()
        .await
        .expect("create");
    assert_eq!(created.status(), StatusCode::CREATED);
    let created: Value = created.json().await.expect("create body");
    let id = created["id"].as_str().expect("id").to_string();

    let write = request(Method::PUT, &format!("/v1/sandboxes/{id}/files/a.txt"))
        .body("hello")
        .send()
        .await
        .expect("write");
    assert_eq!(write.status(), StatusCode::NO_CONTENT);

    let start = request(Method::POST, &format!("/v1/sandboxes/{id}/start"))
        .send()
        .await
        .expect("start");
    assert_eq!(start.status(), StatusCode::NO_CONTENT);

    let exec = request(Method::POST, &format!("/v1/sandboxes/{id}/exec"))
        .json(&json!({
            "argv": ["python3", "-c", "print(6*7)"],
            "timeout_ms": 3000,
            "output_limit_bytes": 4096
        }))
        .send()
        .await
        .expect("exec");
    assert_eq!(exec.status(), StatusCode::OK);
    let output: Value = exec.json().await.expect("exec body");
    assert_eq!(output["exit_code"], 0);
    assert_eq!(output["stdout_base64"], "NDIK");

    let read = request(Method::GET, &format!("/v1/sandboxes/{id}/files/a.txt"))
        .send()
        .await
        .expect("read");
    assert_eq!(read.status(), StatusCode::OK);
    assert_eq!(read.bytes().await.expect("bytes").as_ref(), b"hello");

    let captured = request(Method::POST, &format!("/v1/sandboxes/{id}/snapshots"))
        .json(&json!({
            "max_total_bytes": 1048576,
            "max_entries": 1024,
            "max_archive_bytes": 4194304
        }))
        .send()
        .await
        .expect("capture");
    assert_eq!(captured.status(), StatusCode::CREATED);
    let snapshot: Value = captured.json().await.expect("snapshot");
    let snapshot_id = snapshot["snapshot_id"].as_str().expect("snapshot id");

    let target = request(Method::POST, "/v1/sandboxes")
        .json(&json!({
            "backend": {
                "kind": "docker",
                "endpoint": endpoint,
                "image": image,
                "runtime": "runc"
            },
            "sandbox": {
                "memory_bytes": 67108864,
                "cpu_millis": 1000,
                "pids": 32,
                "workspace_bytes": 8388608,
                "max_file_bytes": 65536,
                "isolation": {
                    "workspace_path": "/workspace",
                    "guest_uid": 65534,
                    "guest_gid": 65534
                },
                "io": {
                    "output_limit_bytes": 65536,
                    "command_timeout_ms": 5000,
                    "file_timeout_ms": 5000,
                    "lifecycle_timeout_ms": 5000
                }
            }
        }))
        .send()
        .await
        .expect("create target");
    assert_eq!(target.status(), StatusCode::CREATED);
    let target: Value = target.json().await.expect("target body");
    let target_id = target["id"].as_str().expect("target id").to_string();

    let restore = request(
        Method::POST,
        &format!("/v1/sandboxes/{target_id}/snapshots/{snapshot_id}/restore"),
    )
    .send()
    .await
    .expect("restore");
    assert_eq!(restore.status(), StatusCode::NO_CONTENT);
    let restored = request(
        Method::GET,
        &format!("/v1/sandboxes/{target_id}/files/a.txt"),
    )
    .send()
    .await
    .expect("read restored");
    assert_eq!(restored.bytes().await.expect("bytes").as_ref(), b"hello");

    for sandbox in [&id, &target_id] {
        let deleted = request(Method::DELETE, &format!("/v1/sandboxes/{sandbox}"))
            .send()
            .await
            .expect("delete");
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    }

    let _ = shutdown.send(());
    let served = handle.await.expect("join").expect("serve");
    assert!(served.report.complete());
    assert_eq!(served.report.sandboxes, 0);
}
