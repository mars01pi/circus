//! Deterministic Docker HTTP boundary tests; no daemon or registry is contacted.
//! The peer is a real Unix socket / Windows named pipe used by the public API's
//! Bollard client, not an injected production implementation.
#![cfg(any(unix, windows))]

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use circus_sandbox::environment::{
    EnvironmentError, ImagePlatform, ImagePullPolicy, ImageRequest, PreparedImage,
    prepare_docker_image,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const REFERENCE: &str = "registry.example/team/image:stable";
const OPERATION_LIMIT: Duration = Duration::from_secs(5);
const GUARD_LIMIT: Duration = Duration::from_secs(10);

#[cfg(unix)]
type Socket = tokio::net::UnixStream;
#[cfg(windows)]
type Socket = tokio::net::windows::named_pipe::NamedPipeServer;

struct Listener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    inner: Socket,
    #[cfg(windows)]
    next: Socket,
    #[cfg(windows)]
    name: String,
}

impl Listener {
    fn bind() -> (Self, String, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("temporary peer directory");
        #[cfg(unix)]
        {
            let path = directory.path().join("engine.sock");
            let inner = tokio::net::UnixListener::bind(&path).expect("bind Unix peer");
            (
                Self { inner },
                format!("unix://{}", path.display()),
                directory,
            )
        }
        #[cfg(windows)]
        {
            let suffix = format!("circus-image-{}", uuid::Uuid::new_v4());
            let name = format!(r"\\.\pipe\{suffix}");
            let inner = tokio::net::windows::named_pipe::ServerOptions::new()
                .first_pipe_instance(true)
                .create(&name)
                .expect("bind named-pipe peer");
            // Bollard's synchronous Path::exists probe opens and closes one
            // instance before Hyper opens the actual HTTP connection. Keep a
            // second instance available even on a single-thread Tokio runtime.
            let next = tokio::net::windows::named_pipe::ServerOptions::new()
                .create(&name)
                .expect("prepare pipe instance after existence probe");
            (
                Self { inner, next, name },
                format!("npipe:////./pipe/{suffix}"),
                directory,
            )
        }
    }

    async fn accept(&mut self) -> Socket {
        #[cfg(unix)]
        {
            self.inner.accept().await.expect("accept Unix peer").0
        }
        #[cfg(windows)]
        {
            self.inner.connect().await.expect("accept named-pipe peer");
            let next = tokio::net::windows::named_pipe::ServerOptions::new()
                .create(&self.name)
                .expect("prepare next pipe instance");
            let ready = std::mem::replace(&mut self.next, next);
            std::mem::replace(&mut self.inner, ready)
        }
    }
}

#[derive(Debug)]
enum Expected {
    Version,
    Inspect(String),
    Pull {
        reference: String,
        platform: String,
        tag: Option<String>,
    },
}

struct Step {
    expected: Expected,
    response: Response,
}

struct Response {
    status: u16,
    prefix: String,
    tail: String,
    gate: Option<Gate>,
}

struct Gate {
    reached: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

struct Control {
    reached: oneshot::Receiver<()>,
    release: oneshot::Sender<()>,
}

impl Response {
    fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            prefix: body.into(),
            tail: String::new(),
            gate: None,
        }
    }

    fn gated(prefix: &str, tail: &str) -> (Self, Control) {
        let (reached, wait_reached) = oneshot::channel();
        let (release, wait_release) = oneshot::channel();
        (
            Self {
                status: 200,
                prefix: prefix.into(),
                tail: tail.into(),
                gate: Some(Gate {
                    reached,
                    release: wait_release,
                }),
            },
            Control {
                reached: wait_reached,
                release,
            },
        )
    }
}

fn version() -> Step {
    Step {
        expected: Expected::Version,
        response: Response::json(200, r#"{"ApiVersion":"1.47","MinAPIVersion":"1.24"}"#),
    }
}

fn inspect(reference: &str, response: Response) -> Step {
    Step {
        expected: Expected::Inspect(reference.into()),
        response,
    }
}

fn pull(reference: &str, platform: &str, tag: Option<&str>, response: Response) -> Step {
    Step {
        expected: Expected::Pull {
            reference: reference.into(),
            platform: platform.into(),
            tag: tag.map(str::to_owned),
        },
        response,
    }
}

fn missing() -> Response {
    Response::json(404, r#"{"message":"No such image"}"#)
}

fn image(os: &str, architecture: &str) -> Response {
    Response::json(
        200,
        json!({
            "Id": format!("sha256:{}", "a".repeat(64)),
            "RepoDigests": [format!("registry.example/team/image@sha256:{}", "b".repeat(64))],
            "Os": os,
            "Architecture": architecture,
        })
        .to_string(),
    )
}

fn progress() -> Response {
    Response::json(
        200,
        "{\"status\":\"Pulling\",\"id\":\"layer\"}\n{\"status\":\"Done\"}\n",
    )
}

fn request(endpoint: &str, reference: &str) -> ImageRequest {
    ImageRequest::new(
        endpoint,
        reference,
        ImagePlatform::new("linux", "amd64").expect("valid platform"),
        OPERATION_LIMIT,
    )
    .expect("valid image request")
}

async fn prepare(request: ImageRequest) -> Result<PreparedImage, EnvironmentError> {
    tokio::time::timeout(GUARD_LIMIT, prepare_docker_image(request))
        .await
        .expect("image preparation must remain bounded")
}

/// All peer work belongs to this handle. Failure/panic/drop aborts the task;
/// normal completion awaits it, including its boundary assertions.
struct Peer {
    endpoint: String,
    task: Option<JoinHandle<()>>,
    stop: Option<oneshot::Sender<()>>,
    _directory: tempfile::TempDir,
}

impl Peer {
    fn new(steps: Vec<Step>) -> Self {
        let (mut listener, endpoint, directory) = Listener::bind();
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut steps: VecDeque<_> = steps.into();
            #[cfg(windows)]
            let mut initial_request = Some(accept_pipe_probe_and_request(&mut listener).await);
            #[cfg(unix)]
            let mut initial_request: Option<(Socket, String)> = None;
            loop {
                let (mut socket, header) = match initial_request.take() {
                    Some(request) => request,
                    None => {
                        let mut socket = tokio::select! {
                            biased;
                            socket = listener.accept() => socket,
                            _ = &mut stopped => break,
                        };
                        let header = read_header(&mut socket).await;
                        (socket, header)
                    }
                };
                let step = steps
                    .pop_front()
                    .unwrap_or_else(|| panic!("unexpected request: {header}"));
                assert_request(&header, &step.expected);
                respond(&mut listener, &mut socket, step.response).await;
            }
            assert!(
                steps.is_empty(),
                "unobserved expected requests: {}",
                steps.len()
            );
            // A detached continuation must not inspect/pull/delete after its
            // caller has returned or dropped the operation. This bounded quiet
            // interval starts only after the explicit completion handshake.
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "follow-up connection after image operation completed"
            );
        });
        Self {
            endpoint,
            task: Some(task),
            stop: Some(stop),
            _directory: directory,
        }
    }

    async fn finish(mut self) {
        let _ = self.stop.take().expect("stop sender").send(());
        tokio::time::timeout(GUARD_LIMIT, self.task.as_mut().expect("peer task"))
            .await
            .expect("peer shutdown must remain bounded")
            .expect("peer boundary assertions");
        self.task.take();
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[cfg(windows)]
async fn accept_pipe_probe_and_request(listener: &mut Listener) -> (Socket, String) {
    // Windows may choose either pending instance. Require exactly one empty
    // metadata-probe connection and one complete HTTP request, without assuming
    // their accept order or tolerating EOF in any partial request.
    let mut first = listener.accept().await;
    let mut second = listener.accept().await;
    let (first_header, second_header) = tokio::join!(
        read_header_or_probe(&mut first),
        read_header_or_probe(&mut second)
    );
    match (first_header, second_header) {
        (Some(header), None) => (first, header),
        (None, Some(header)) => (second, header),
        _ => panic!("expected one empty Bollard existence probe and one HTTP request"),
    }
}

async fn read_header(socket: &mut Socket) -> String {
    read_header_or_probe(socket)
        .await
        .expect("HTTP connection closed without a request")
}

async fn read_header_or_probe(socket: &mut Socket) -> Option<String> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0];
        if socket
            .read(&mut byte)
            .await
            .expect("read HTTP request header")
            == 0
        {
            assert!(
                bytes.is_empty(),
                "HTTP connection closed during a partial request"
            );
            return None;
        }
        bytes.push(byte[0]);
        assert!(bytes.len() <= 32 * 1024, "oversized HTTP request header");
        if bytes.ends_with(b"\r\n\r\n") {
            return Some(String::from_utf8(bytes).expect("ASCII HTTP request"));
        }
    }
}

fn assert_request(header: &str, expected: &Expected) {
    let mut lines = header.split("\r\n");
    let first = lines.next().expect("request line");
    let parts: Vec<_> = first.split_whitespace().collect();
    assert_eq!(parts.len(), 3, "{first}");
    assert_eq!(parts[2], "HTTP/1.1");
    let url = url::Url::parse(&format!("http://peer{}", parts[1])).expect("request URL");
    // Bollard currently uses absolute, unversioned paths. Also accept the
    // negotiated prefix if the SDK starts preserving it in a later release.
    let path = url.path().strip_prefix("/v1.47").unwrap_or(url.path());
    let query: BTreeMap<_, _> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    match expected {
        Expected::Version => {
            assert_eq!(parts[0], "GET");
            assert_eq!(path, "/version");
            assert!(query.is_empty());
        }
        Expected::Inspect(reference) => {
            assert_eq!(parts[0], "GET");
            let encoded = path
                .strip_prefix("/images/")
                .and_then(|path| path.strip_suffix("/json"))
                .expect("image inspect path");
            let decoded = url::form_urlencoded::parse(format!("reference={encoded}").as_bytes())
                .next()
                .expect("decoded reference")
                .1
                .into_owned();
            assert_eq!(
                &decoded, reference,
                "inspect must retain original reference"
            );
            assert!(query.is_empty());
        }
        Expected::Pull {
            reference,
            platform,
            tag,
        } => {
            assert_eq!(parts[0], "POST");
            assert_eq!(path, "/images/create");
            assert_eq!(query.get("fromImage"), Some(reference));
            assert_eq!(query.get("platform"), Some(platform));
            assert_eq!(
                query.get("tag"),
                tag.as_ref(),
                "never request all tags or rewrite tagged/digested references"
            );
            assert!(
                query
                    .keys()
                    .all(|key| matches!(key.as_str(), "fromImage" | "platform" | "tag")),
                "unexpected pull options: {query:?}"
            );
        }
    }
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').expect("HTTP header field");
        match name.to_ascii_lowercase().as_str() {
            "authorization" | "proxy-authorization" | "cookie" | "x-registry-config" => {
                panic!("credential-bearing header must not be sent: {name}")
            }
            "x-registry-auth" => {
                // SDKs may encode absent credentials as {} or null. Neither
                // contains a username, password, token, or identity secret.
                assert!(
                    matches!(value.trim(), "" | "e30=" | "e30" | "bnVsbA==" | "bnVsbA"),
                    "nonempty registry credentials"
                );
            }
            _ => {}
        }
    }
}

async fn respond(listener: &mut Listener, socket: &mut Socket, response: Response) {
    let header = format!(
        "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.prefix.len() + response.tail.len(),
    );
    socket
        .write_all(header.as_bytes())
        .await
        .expect("response header");
    socket
        .write_all(response.prefix.as_bytes())
        .await
        .expect("response prefix");
    socket.flush().await.expect("flush response prefix");
    if let Some(mut gate) = response.gate {
        let _ = gate.reached.send(());
        tokio::select! {
            biased;
            _ = listener.accept() => panic!("follow-up request before current response reached EOF"),
            _ = &mut gate.release => {}
        }
        // A timed-out/dropped client may already have closed this connection.
        let _ = socket.write_all(response.tail.as_bytes()).await;
        let _ = socket.shutdown().await;
    } else {
        socket
            .write_all(response.tail.as_bytes())
            .await
            .expect("response tail");
        socket.shutdown().await.expect("close response");
    }
}

fn assert_image(prepared: &PreparedImage, architecture: &str) {
    assert_eq!(prepared.id(), format!("sha256:{}", "a".repeat(64)));
    assert_eq!(prepared.os(), "linux");
    assert_eq!(prepared.architecture(), architecture);
    assert_eq!(
        prepared.repo_digests(),
        &[format!(
            "registry.example/team/image@sha256:{}",
            "b".repeat(64)
        )]
    );
}

#[tokio::test]
async fn default_and_explicit_local_only_misses_never_pull() {
    for explicit in [false, true] {
        let peer = Peer::new(vec![version(), inspect(REFERENCE, missing())]);
        let mut request = request(&peer.endpoint, REFERENCE);
        if explicit {
            request = request.with_pull_policy(ImagePullPolicy::LocalOnly);
        }
        assert!(
            matches!(prepare(request).await, Err(EnvironmentError::ImageNotFound { reference }) if reference == REFERENCE)
        );
        peer.finish().await;
    }
}

#[tokio::test]
async fn cached_if_missing_success_and_mismatches_never_pull() {
    for (os, arch) in [("linux", "amd64"), ("linux", "arm64"), ("windows", "amd64")] {
        let peer = Peer::new(vec![version(), inspect(REFERENCE, image(os, arch))]);
        let result = prepare(
            request(&peer.endpoint, REFERENCE).with_pull_policy(ImagePullPolicy::IfMissing),
        )
        .await;
        if (os, arch) == ("linux", "amd64") {
            assert_image(&result.expect("cached image"), arch);
        } else {
            assert!(
                matches!(result, Err(EnvironmentError::PlatformMismatch { actual_os, actual_architecture, expected }) if actual_os == os && actual_architecture == arch && expected == ImagePlatform::new("linux", "amd64").unwrap())
            );
        }
        peer.finish().await;
    }
}

#[tokio::test]
async fn initial_http_failures_do_not_trigger_pull() {
    for status in [403, 500] {
        for at_version in [false, true] {
            let failure = Response::json(status, r#"{"message":"permission or daemon failure"}"#);
            let steps = if at_version {
                vec![Step {
                    expected: Expected::Version,
                    response: failure,
                }]
            } else {
                vec![version(), inspect(REFERENCE, failure)]
            };
            let peer = Peer::new(steps);
            assert!(matches!(
                prepare(
                    request(&peer.endpoint, REFERENCE).with_pull_policy(ImagePullPolicy::IfMissing)
                )
                .await,
                Err(EnvironmentError::Engine { .. })
            ));
            peer.finish().await;
        }
    }
}

#[tokio::test]
async fn unavailable_explicit_socket_returns_engine_error() {
    let (listener, endpoint, _directory) = Listener::bind();
    drop(listener);
    assert!(matches!(
        prepare(request(&endpoint, REFERENCE).with_pull_policy(ImagePullPolicy::IfMissing)).await,
        Err(EnvironmentError::Engine { .. })
    ));
}

#[tokio::test]
async fn missing_references_preserve_pull_query_and_final_inspect_identity() {
    let digest = format!("registry.example:5000/team/image@sha256:{}", "c".repeat(64));
    for (reference, tag, variant) in [
        ("registry.example:5000/team/image:stable", None, None),
        (digest.as_str(), None, Some("v8")),
        ("image", Some("latest"), None),
        (
            "registry.example:5000/team/image",
            Some("latest"),
            Some("v8"),
        ),
    ] {
        let platform = if variant.is_some() {
            "linux/arm64/v8"
        } else {
            "linux/arm64"
        };
        let peer = Peer::new(vec![
            version(),
            inspect(reference, missing()),
            pull(reference, platform, tag, progress()),
            inspect(reference, image("linux", "arm64")),
        ]);
        let request = ImageRequest::new(
            &peer.endpoint,
            reference,
            ImagePlatform::with_variant("linux", "arm64", variant.map(str::to_owned)).unwrap(),
            OPERATION_LIMIT,
        )
        .unwrap()
        .with_pull_policy(ImagePullPolicy::IfMissing);
        assert_image(&prepare(request).await.expect("pulled image"), "arm64");
        peer.finish().await;
    }
}

#[tokio::test]
async fn pull_progress_is_consumed_until_eof_before_final_inspect() {
    let (response, control) =
        Response::gated("{\"status\":\"Pulling\"}\n", "{\"status\":\"Complete\"}\n");
    let peer = Peer::new(vec![
        version(),
        inspect(REFERENCE, missing()),
        pull(REFERENCE, "linux/amd64", None, response),
        inspect(REFERENCE, image("linux", "amd64")),
    ]);
    let mut future = Box::pin(prepare(
        request(&peer.endpoint, REFERENCE).with_pull_policy(ImagePullPolicy::IfMissing),
    ));
    tokio::select! {
        result = &mut future => panic!("operation finished before pull EOF: {result:?}"),
        reached = control.reached => reached.expect("peer reached progress barrier"),
    }
    // The peer concurrently rejects any new connection while the last frame
    // and HTTP EOF remain withheld, including an eager final inspection.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut future)
            .await
            .is_err(),
        "preparation must wait for the rest of the progress stream"
    );
    control.release.send(()).expect("release progress");
    assert_image(&future.await.expect("complete pull"), "amd64");
    peer.finish().await;
}

#[tokio::test]
async fn canonical_in_band_errors_and_malformed_json_stop_before_final_inspect() {
    for body in [
        "{\"errorDetail\":{\"message\":\"registry denied pull\"}}\n",
        "{\"errorDetail\":{}}\n",
        "{\"errorDetail\":{\"code\":403}}\n",
        "{\"status\":\"pulling\",\"errorDetail\":{}}\n",
        "{\"status\":\"pulling\",\"errorDetail\":{\"code\":403}}\n",
        "{\"errorDetail\":{\"message\":\"\"}}\n",
        "{\"errorDetail\":{\"message\":\"   \"}}\n",
        "{\"status\":\"Downloading\"}\nnot-json\n",
        "{\"status\":false}\n",
    ] {
        let peer = Peer::new(vec![
            version(),
            inspect(REFERENCE, missing()),
            pull(REFERENCE, "linux/amd64", None, Response::json(200, body)),
        ]);
        let error = prepare(
            request(&peer.endpoint, REFERENCE).with_pull_policy(ImagePullPolicy::IfMissing),
        )
        .await
        .expect_err(body);
        assert!(
            matches!(error, EnvironmentError::Engine { .. }),
            "{body}: {error:?}"
        );
        if body.contains("registry denied pull") {
            assert!(error.to_string().contains("registry denied pull"));
        }
        peer.finish().await;
    }
}

#[tokio::test]
async fn first_in_band_error_stops_without_waiting_for_stream_eof() {
    let (response, control) = Response::gated(
        "{\"errorDetail\":{\"message\":\"denied before EOF\"}}\n",
        "{\"status\":\"never needed\"}\n",
    );
    let peer = Peer::new(vec![
        version(),
        inspect(REFERENCE, missing()),
        pull(REFERENCE, "linux/amd64", None, response),
    ]);
    let mut future = Box::pin(prepare(
        request(&peer.endpoint, REFERENCE).with_pull_policy(ImagePullPolicy::IfMissing),
    ));
    tokio::select! {
        biased;
        reached = control.reached => reached.expect("peer error barrier"),
        result = &mut future => panic!("peer barrier must precede result: {result:?}"),
    }
    let result = tokio::time::timeout(Duration::from_secs(1), &mut future)
        .await
        .expect("first error does not await EOF");
    assert!(
        matches!(result, Err(EnvironmentError::Engine { message }) if message.contains("denied before EOF"))
    );
    control
        .release
        .send(())
        .expect("release closed error stream");
    peer.finish().await;
}

#[tokio::test]
async fn http_pull_errors_do_not_reinspect_or_retry() {
    for status in [401, 403, 404, 500] {
        let peer = Peer::new(vec![
            version(),
            inspect(REFERENCE, missing()),
            pull(
                REFERENCE,
                "linux/amd64",
                None,
                Response::json(status, r#"{"message":"pull failed"}"#),
            ),
        ]);
        assert!(matches!(
            prepare(
                request(&peer.endpoint, REFERENCE).with_pull_policy(ImagePullPolicy::IfMissing)
            )
            .await,
            Err(EnvironmentError::Engine { .. })
        ));
        peer.finish().await;
    }
}

#[tokio::test]
async fn final_inspect_is_authoritative_and_returns_typed_failures() {
    for scenario in ["architecture", "os", "missing", "forbidden", "daemon"] {
        let final_response = match scenario {
            "architecture" => image("linux", "arm64"),
            "os" => image("windows", "amd64"),
            "missing" => missing(),
            "forbidden" => Response::json(403, r#"{"message":"inspect denied"}"#),
            "daemon" => Response::json(500, r#"{"message":"daemon failed"}"#),
            _ => unreachable!(),
        };
        let peer = Peer::new(vec![
            version(),
            inspect(REFERENCE, missing()),
            pull(REFERENCE, "linux/amd64", None, progress()),
            inspect(REFERENCE, final_response),
        ]);
        let result = prepare(
            request(&peer.endpoint, REFERENCE).with_pull_policy(ImagePullPolicy::IfMissing),
        )
        .await;
        match scenario {
            "architecture" => assert!(
                matches!(result, Err(EnvironmentError::PlatformMismatch { actual_architecture, .. }) if actual_architecture == "arm64")
            ),
            "os" => assert!(
                matches!(result, Err(EnvironmentError::PlatformMismatch { actual_os, .. }) if actual_os == "windows")
            ),
            "missing" => assert!(
                matches!(result, Err(EnvironmentError::ImageNotFound { reference }) if reference == REFERENCE)
            ),
            _ => assert!(matches!(result, Err(EnvironmentError::Engine { .. }))),
        }
        peer.finish().await;
    }
}

fn stalled_scenario(stage: &str) -> (Vec<Step>, Control) {
    let complete = match stage {
        "version" => r#"{"ApiVersion":"1.47"}"#.to_owned(),
        "inspect" => missing().prefix,
        "pull" => "{\"status\":\"Done\"}\n".into(),
        "reinspect" => image("linux", "amd64").prefix,
        _ => unreachable!(),
    };
    let (mut stalled, control) = Response::gated("", &complete);
    if stage == "inspect" {
        // A detached continuation would now attempt a pull after cancellation.
        stalled.status = 404;
    }
    let steps = match stage {
        "version" => vec![Step {
            expected: Expected::Version,
            response: stalled,
        }],
        "inspect" => vec![version(), inspect(REFERENCE, stalled)],
        "pull" => vec![
            version(),
            inspect(REFERENCE, missing()),
            pull(REFERENCE, "linux/amd64", None, stalled),
        ],
        "reinspect" => vec![
            version(),
            inspect(REFERENCE, missing()),
            pull(REFERENCE, "linux/amd64", None, progress()),
            inspect(REFERENCE, stalled),
        ],
        _ => unreachable!(),
    };
    (steps, control)
}

#[tokio::test]
async fn whole_operation_timeout_covers_negotiation_inspect_pull_and_reinspect() {
    for stage in ["version", "inspect", "pull", "reinspect"] {
        let (steps, control) = stalled_scenario(stage);
        let peer = Peer::new(steps);
        let request = ImageRequest::new(
            &peer.endpoint,
            REFERENCE,
            ImagePlatform::new("linux", "amd64").unwrap(),
            Duration::from_secs(1),
        )
        .unwrap()
        .with_pull_policy(ImagePullPolicy::IfMissing);
        let mut future = Box::pin(prepare(request));
        tokio::select! {
            biased;
            reached = control.reached => reached.expect("stalled request reached"),
            result = &mut future => panic!("deadline elapsed before synchronized {stage} barrier: {result:?}"),
        }
        assert!(
            matches!(future.await, Err(EnvironmentError::Timeout)),
            "stage {stage}"
        );
        control.release.send(()).expect("release timed-out request");
        peer.finish().await;
    }
}

#[tokio::test]
async fn dropping_preparation_at_each_boundary_prevents_follow_up_requests() {
    for stage in ["version", "inspect", "pull", "reinspect"] {
        let (steps, control) = stalled_scenario(stage);
        let peer = Peer::new(steps);
        let mut future = Box::pin(prepare(
            request(&peer.endpoint, REFERENCE).with_pull_policy(ImagePullPolicy::IfMissing),
        ));
        tokio::select! {
            biased;
            reached = control.reached => reached.expect("stalled request reached"),
            result = &mut future => panic!("operation unexpectedly completed at {stage}: {result:?}"),
        }
        drop(future);
        control.release.send(()).expect("release cancelled request");
        peer.finish().await;
    }
}
