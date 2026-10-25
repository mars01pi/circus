//! Minimal concrete Images, Content and Transfer services, using SDK messages.
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use containerd_client::services::v1::{
    CreateImageRequest, CreateImageResponse, GetImageRequest, GetImageResponse, Image,
    ReadContentRequest, ReadContentResponse, TransferRequest,
};
use containerd_client::tonic::codegen::{BoxFuture, Service, http};
use containerd_client::tonic::server::{NamedService, ServerStreamingService, UnaryService};
use containerd_client::tonic::{self, Request, Response, Status};
use containerd_client::types::Descriptor;
use futures_util::stream::{self, BoxStream};
use tonic_prost::ProstCodec;

pub const NAMESPACE: &str = "circus-policy";
pub const REFERENCE: &str = "registry.example/team/image:stable";

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Get(String),
    Transfer(TransferRequest),
    Read(String),
    Create(Image),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Hang {
    Transfer,
    Read,
    Alias,
}

pub struct TransferGate {
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
    finished: tokio::sync::oneshot::Sender<()>,
}

pub struct TransferControl {
    pub entered: tokio::sync::oneshot::Receiver<()>,
    pub release: tokio::sync::oneshot::Sender<()>,
    pub finished: tokio::sync::oneshot::Receiver<()>,
}

struct TransferFinished(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for TransferFinished {
    fn drop(&mut self) {
        if let Some(finished) = self.0.take() {
            let _ = finished.send(());
        }
    }
}

pub struct State {
    pub image: Image,
    pub blobs: BTreeMap<String, Vec<u8>>,
    pub missing: bool,
    pub lookup_error: Option<tonic::Code>,
    pub existing_alias: Option<Descriptor>,
    pub hang: Option<Hang>,
    pub transfer_gate: Option<TransferGate>,
    pub events: Vec<Event>,
    pub aliases: BTreeMap<String, Descriptor>,
}

pub struct Peer {
    pub socket: String,
    pub state: Arc<Mutex<State>>,
    task: tokio::task::JoinHandle<()>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    _directory: tempfile::TempDir,
}

impl Peer {
    pub fn start(state: State) -> Self {
        let directory = tempfile::tempdir().expect("peer temporary directory");
        let path = directory.path().join("containerd.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind peer socket");
        let state = Arc::new(Mutex::new(state));
        let service = PeerService(state.clone());
        let incoming = stream::unfold(listener, |listener| async {
            let next = listener.accept().await.map(|(stream, _)| stream);
            Some((next, listener))
        });
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .serve_with_incoming_shutdown(service, incoming, async {
                    let _ = stopped.await;
                })
                .await
                .expect("serve containerd peer");
        });
        Self {
            socket: path.to_str().expect("UTF-8 socket").into(),
            state,
            task,
            shutdown: Some(shutdown),
            _directory: directory,
        }
    }

    pub fn events(&self) -> Vec<Event> {
        self.state.lock().expect("peer state").events.clone()
    }

    pub fn gate_transfer(&self) -> TransferControl {
        let (entered, wait_entered) = tokio::sync::oneshot::channel();
        let (release, wait_release) = tokio::sync::oneshot::channel();
        let (finished, wait_finished) = tokio::sync::oneshot::channel();
        self.state.lock().expect("peer state").transfer_gate = Some(TransferGate {
            entered,
            release: wait_release,
            finished,
        });
        TransferControl {
            entered: wait_entered,
            release,
            finished: wait_finished,
        }
    }

    pub async fn finish(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        (&mut self.task).await.expect("peer shutdown");
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
struct PeerService(Arc<Mutex<State>>);

impl Service<http::Request<tonic::body::Body>> for PeerService {
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
        let path = request.uri().path();
        if path.starts_with("/containerd.services.images.v1.Images/") {
            Images(self.0.clone()).call(request)
        } else if path.starts_with("/containerd.services.content.v1.Content/") {
            Content(self.0.clone()).call(request)
        } else if path.starts_with("/containerd.services.transfer.v1.Transfer/") {
            Transfer(self.0.clone()).call(request)
        } else {
            Box::pin(async {
                Ok(Status::unimplemented("unexpected containerd service").into_http())
            })
        }
    }
}

fn namespace<T>(request: &Request<T>) -> Result<(), Status> {
    if request
        .metadata()
        .get("containerd-namespace")
        .and_then(|v| v.to_str().ok())
        != Some(NAMESPACE)
    {
        return Err(Status::permission_denied("wrong namespace"));
    }
    Ok(())
}

#[derive(Clone)]
struct Get(Arc<Mutex<State>>);
impl UnaryService<GetImageRequest> for Get {
    type Response = GetImageResponse;
    type Future = BoxFuture<Response<Self::Response>, Status>;
    fn call(&mut self, request: Request<GetImageRequest>) -> Self::Future {
        let state = self.0.clone();
        Box::pin(async move {
            namespace(&request)?;
            let name = request.into_inner().name;
            let mut state = state.lock().expect("peer state");
            state.events.push(Event::Get(name.clone()));
            if let Some(code) = state.lookup_error {
                return Err(Status::new(code, "secret credential must not escape"));
            }
            if name == state.image.name {
                if state.missing {
                    return Err(Status::not_found("missing image"));
                }
                return Ok(Response::new(GetImageResponse {
                    image: Some(state.image.clone()),
                }));
            }
            if let Some(target) = state.aliases.get(&name).cloned() {
                return Ok(Response::new(GetImageResponse {
                    image: Some(Image {
                        name,
                        target: Some(target),
                        ..Default::default()
                    }),
                }));
            }
            if let Some(target) = state.existing_alias.clone() {
                return Ok(Response::new(GetImageResponse {
                    image: Some(Image {
                        name,
                        target: Some(target),
                        ..Default::default()
                    }),
                }));
            }
            Err(Status::not_found("missing alias"))
        })
    }
}

#[derive(Clone)]
struct Create(Arc<Mutex<State>>);
impl UnaryService<CreateImageRequest> for Create {
    type Response = CreateImageResponse;
    type Future = BoxFuture<Response<Self::Response>, Status>;
    fn call(&mut self, request: Request<CreateImageRequest>) -> Self::Future {
        let shared = self.0.clone();
        Box::pin(async move {
            namespace(&request)?;
            let image = request
                .into_inner()
                .image
                .ok_or_else(|| Status::invalid_argument("image"))?;
            let hang = {
                let mut state = shared.lock().expect("peer state");
                state.events.push(Event::Create(image.clone()));
                state.hang == Some(Hang::Alias)
            };
            if hang {
                std::future::pending::<()>().await;
            }
            let mut state = shared.lock().expect("peer state");
            if state.existing_alias.is_some() {
                return Err(Status::already_exists("alias exists"));
            }
            let target = image
                .target
                .clone()
                .ok_or_else(|| Status::invalid_argument("target"))?;
            state.aliases.insert(image.name.clone(), target);
            Ok(Response::new(CreateImageResponse { image: Some(image) }))
        })
    }
}

#[derive(Clone)]
struct Read(Arc<Mutex<State>>);
impl ServerStreamingService<ReadContentRequest> for Read {
    type Response = ReadContentResponse;
    type ResponseStream = BoxStream<'static, Result<Self::Response, Status>>;
    type Future = BoxFuture<Response<Self::ResponseStream>, Status>;
    fn call(&mut self, request: Request<ReadContentRequest>) -> Self::Future {
        let shared = self.0.clone();
        Box::pin(async move {
            namespace(&request)?;
            let request = request.into_inner();
            let (bytes, hang) = {
                let mut state = shared.lock().expect("peer state");
                state.events.push(Event::Read(request.digest.clone()));
                let bytes = state
                    .blobs
                    .get(&request.digest)
                    .cloned()
                    .ok_or_else(|| Status::not_found("missing blob"))?;
                (bytes, state.hang == Some(Hang::Read))
            };
            // Delay the body, not just the headers: the public deadline must
            // also encompass a stalled streaming metadata response.
            let output: Self::ResponseStream = Box::pin(stream::unfold(
                (bytes, 0usize, hang),
                |(bytes, offset, hang)| async move {
                    if hang {
                        std::future::pending::<()>().await;
                    }
                    if offset == bytes.len() {
                        return None;
                    }
                    let end = (offset + 1024 * 1024).min(bytes.len());
                    let message = ReadContentResponse {
                        offset: i64::try_from(offset).unwrap(),
                        data: bytes[offset..end].to_vec(),
                    };
                    Some((Ok(message), (bytes, end, false)))
                },
            ));
            Ok(Response::new(output))
        })
    }
}

#[derive(Clone)]
struct Pull(Arc<Mutex<State>>);
impl UnaryService<TransferRequest> for Pull {
    type Response = ();
    type Future = BoxFuture<Response<()>, Status>;
    fn call(&mut self, request: Request<TransferRequest>) -> Self::Future {
        let shared = self.0.clone();
        Box::pin(async move {
            namespace(&request)?;
            let (hang, gate) = {
                let mut state = shared.lock().expect("peer state");
                state.events.push(Event::Transfer(request.into_inner()));
                (
                    state.hang == Some(Hang::Transfer),
                    state.transfer_gate.take(),
                )
            };
            let _finished = if let Some(gate) = gate {
                let finished = TransferFinished(Some(gate.finished));
                let _ = gate.entered.send(());
                let _ = gate.release.await;
                Some(finished)
            } else {
                None
            };
            if hang {
                std::future::pending::<()>().await;
            }
            shared.lock().expect("peer state").missing = false;
            Ok(Response::new(()))
        })
    }
}

#[derive(Clone)]
struct Images(Arc<Mutex<State>>);
impl NamedService for Images {
    const NAME: &'static str = "containerd.services.images.v1.Images";
}
impl Service<http::Request<tonic::body::Body>> for Images {
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
        let state = self.0.clone();
        Box::pin(async move {
            let mut grpc = tonic::server::Grpc::new(ProstCodec::default());
            match request.uri().path() {
                "/containerd.services.images.v1.Images/Get" => {
                    Ok(grpc.unary(Get(state), request).await)
                }
                "/containerd.services.images.v1.Images/Create" => {
                    let mut grpc = tonic::server::Grpc::new(ProstCodec::default());
                    Ok(grpc.unary(Create(state), request).await)
                }
                _ => Ok(Status::unimplemented("unexpected image mutation").into_http()),
            }
        })
    }
}

#[derive(Clone)]
struct Content(Arc<Mutex<State>>);
impl NamedService for Content {
    const NAME: &'static str = "containerd.services.content.v1.Content";
}
impl Service<http::Request<tonic::body::Body>> for Content {
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
        let state = self.0.clone();
        Box::pin(async move {
            if request.uri().path() != "/containerd.services.content.v1.Content/Read" {
                return Ok(Status::unimplemented("unexpected content operation").into_http());
            }
            let mut grpc = tonic::server::Grpc::new(ProstCodec::default());
            Ok(grpc.server_streaming(Read(state), request).await)
        })
    }
}

#[derive(Clone)]
struct Transfer(Arc<Mutex<State>>);
impl NamedService for Transfer {
    const NAME: &'static str = "containerd.services.transfer.v1.Transfer";
}
impl Service<http::Request<tonic::body::Body>> for Transfer {
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
        let state = self.0.clone();
        Box::pin(async move {
            if request.uri().path() != "/containerd.services.transfer.v1.Transfer/Transfer" {
                return Ok(Status::unimplemented("unexpected transfer operation").into_http());
            }
            let mut grpc = tonic::server::Grpc::new(ProstCodec::default());
            Ok(grpc.unary(Pull(state), request).await)
        })
    }
}
