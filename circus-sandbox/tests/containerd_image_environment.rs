//! Opt-in real containerd image preparation, immutable alias, and guest execution.
//!
//! Uses the existing CIRCUS_CONTAINERD_SOCKET / CIRCUS_CONTAINERD_IMAGE overrides,
//! but never the shared CIRCUS_CONTAINERD_NAMESPACE. A fresh, labelled namespace
//! owns every image and snapshot touched here. No ctr or content-delete RPC runs.
//! Requires a Linux daemon with anonymous registry transfer, runc and overlayfs,
//! and a publicly pullable Python 3 image for the requested native platform.

#![cfg(all(target_os = "linux", feature = "containerd"))]

mod containerd_support;

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use circus_sandbox::environment::{
    ContainerdImageError, ContainerdImageRequest, ImagePlatform, ImagePullPolicy,
    prepare_containerd_image,
};
use circus_sandbox::{BackendConfig, Sandbox, SandboxPhase};
use containerd_client::services::v1::{
    CreateNamespaceRequest, DeleteImageRequest, DeleteNamespaceRequest, GetContainerRequest,
    GetImageRequest, GetNamespaceRequest, GetRequest, Image, ListContainersRequest,
    ListImagesRequest, ListNamespacesRequest, ListTasksRequest, Namespace, ReadContentRequest,
    containers_client::ContainersClient,
    content_client::ContentClient,
    images_client::ImagesClient,
    namespaces_client::NamespacesClient,
    snapshots::{
        Info, ListSnapshotsRequest, RemoveSnapshotRequest, StatSnapshotRequest,
        snapshots_client::SnapshotsClient,
    },
    tasks_client::TasksClient,
};
use containerd_client::tonic::transport::Channel;
use containerd_client::tonic::{Code, Request};
use containerd_client::types::Descriptor;
use containerd_client::with_namespace;
use containerd_support as support;
use futures_util::FutureExt;

const OWNER_LABEL: &str = "circus.test.containerd-image-owner";
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;

struct Fixture {
    channel: Channel,
    namespace: String,
    owner: String,
}

impl Fixture {
    async fn create(socket: &str) -> support::Result<Self> {
        let owner = uuid::Uuid::new_v4().to_string();
        let namespace = format!("circus-image-{}-{owner}", std::process::id());
        let channel =
            tokio::time::timeout(RPC_TIMEOUT, containerd_client::connect(socket)).await??;
        let mut request = Request::new(CreateNamespaceRequest {
            namespace: Some(Namespace {
                name: namespace.clone(),
                labels: HashMap::from([(OWNER_LABEL.to_string(), owner.clone())]),
            }),
        });
        request.set_timeout(RPC_TIMEOUT);
        // AlreadyExists is a failure: never adopt an existing namespace.
        NamespacesClient::new(channel.clone())
            .create(request)
            .await?;
        Ok(Self {
            channel,
            namespace,
            owner,
        })
    }

    fn request<T>(&self, message: T) -> Request<T> {
        let mut request = with_namespace!(message, self.namespace.as_str());
        request.set_timeout(RPC_TIMEOUT);
        request
    }

    async fn image(&self, reference: &str) -> support::Result<Image> {
        Ok(ImagesClient::new(self.channel.clone())
            .get(self.request(GetImageRequest {
                name: reference.to_string(),
            }))
            .await?
            .into_inner()
            .image
            .ok_or("GetImage returned no image")?)
    }

    async fn assert_image_absent(&self, reference: &str) -> support::Result {
        let result = ImagesClient::new(self.channel.clone())
            .get(self.request(GetImageRequest {
                name: reference.to_string(),
            }))
            .await;
        match result {
            Err(status) if status.code() == Code::NotFound => Ok(()),
            Err(status) => Err(status.into()),
            Ok(_) => Err("image unexpectedly exists in the fresh namespace".into()),
        }
    }

    async fn content(&self, descriptor: &Descriptor) -> support::Result<serde_json::Value> {
        if descriptor.size <= 0 || descriptor.size > MAX_METADATA_BYTES as i64 {
            return Err("metadata descriptor exceeds test read budget".into());
        }
        let mut stream = ContentClient::new(self.channel.clone())
            .read(self.request(ReadContentRequest {
                digest: descriptor.digest.clone(),
                offset: 0,
                size: descriptor.size,
            }))
            .await?
            .into_inner();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.message().await? {
            if chunk.offset != bytes.len() as i64
                || chunk.data.len() > MAX_METADATA_BYTES - bytes.len()
            {
                return Err("invalid or oversized content stream".into());
            }
            bytes.extend_from_slice(&chunk.data);
        }
        if bytes.len() as i64 != descriptor.size {
            return Err("metadata content size differs from its descriptor".into());
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn assert_selected_manifest(
        &self,
        source: &str,
        alias: &str,
        digest: &str,
        platform: &ImagePlatform,
    ) -> support::Result {
        let target = self
            .image(alias)
            .await?
            .target
            .ok_or("alias has no target")?;
        assert_eq!(target.digest, digest);
        assert!(matches!(
            target.media_type.as_str(),
            "application/vnd.oci.image.manifest.v1+json"
                | "application/vnd.docker.distribution.manifest.v2+json"
        ));
        let original = self
            .image(source)
            .await?
            .target
            .ok_or("source has no target")?;
        let source_json = self.content(&original).await?;
        let mut index_variant = None;
        if let Some(manifests) = source_json
            .get("manifests")
            .and_then(|value| value.as_array())
        {
            let selected = manifests
                .iter()
                .find(|value| value["digest"].as_str() == Some(digest))
                .ok_or("alias does not target a manifest from the pulled index")?;
            assert_eq!(
                selected["mediaType"].as_str(),
                Some(target.media_type.as_str())
            );
            assert_eq!(selected["size"].as_i64(), Some(target.size));
            assert_eq!(selected["platform"]["os"].as_str(), Some(platform.os()));
            assert_eq!(
                selected["platform"]["architecture"].as_str(),
                Some(platform.architecture())
            );
            index_variant = selected["platform"]["variant"].as_str().map(str::to_string);
            if let Some(variant) = index_variant.as_deref() {
                assert_eq!(platform.variant(), Some(variant));
            }
        } else {
            assert_eq!(original, target);
        }
        // Inspect the selected manifest's config, not just the index or result.
        let manifest = self.content(&target).await?;
        let config = &manifest["config"];
        let config = self
            .content(&Descriptor {
                media_type: config["mediaType"]
                    .as_str()
                    .ok_or("config media type missing")?
                    .into(),
                digest: config["digest"]
                    .as_str()
                    .ok_or("config digest missing")?
                    .into(),
                size: config["size"].as_i64().ok_or("config size missing")?,
                annotations: HashMap::new(),
            })
            .await?;
        assert_eq!(config["os"].as_str(), Some(platform.os()));
        assert_eq!(
            config["architecture"].as_str(),
            Some(platform.architecture())
        );
        assert_eq!(
            config["variant"].as_str().or(index_variant.as_deref()),
            platform.variant()
        );
        Ok(())
    }

    async fn assert_destroyed(&self, id: &str) -> support::Result {
        let container = ContainersClient::new(self.channel.clone())
            .get(self.request(GetContainerRequest { id: id.to_string() }))
            .await;
        assert_eq!(
            container
                .err()
                .ok_or("container remains after destroy")?
                .code(),
            Code::NotFound
        );
        let task = TasksClient::new(self.channel.clone())
            .get(self.request(GetRequest {
                container_id: id.to_string(),
                exec_id: String::new(),
            }))
            .await;
        assert_eq!(
            task.err().ok_or("task remains after destroy")?.code(),
            Code::NotFound
        );
        let snapshot = SnapshotsClient::new(self.channel.clone())
            .stat(self.request(StatSnapshotRequest {
                snapshotter: "overlayfs".to_string(),
                key: id.to_string(),
            }))
            .await;
        assert_eq!(
            snapshot
                .err()
                .ok_or("sandbox snapshot remains after destroy")?
                .code(),
            Code::NotFound
        );
        Ok(())
    }

    async fn snapshots(&self) -> support::Result<Vec<Info>> {
        let mut stream = SnapshotsClient::new(self.channel.clone())
            .list(self.request(ListSnapshotsRequest {
                snapshotter: "overlayfs".to_string(),
                filters: Vec::new(),
            }))
            .await?
            .into_inner();
        let mut snapshots = Vec::new();
        while let Some(batch) = stream.message().await? {
            snapshots.extend(batch.info);
        }
        Ok(snapshots)
    }

    async fn cleanup(&self) -> support::Result {
        let mut namespaces = NamespacesClient::new(self.channel.clone());
        let mut request = Request::new(GetNamespaceRequest {
            name: self.namespace.clone(),
        });
        request.set_timeout(RPC_TIMEOUT);
        let namespace = namespaces
            .get(request)
            .await?
            .into_inner()
            .namespace
            .ok_or("namespace missing")?;
        if namespace.labels.get(OWNER_LABEL) != Some(&self.owner) {
            return Err("namespace ownership label changed; refusing cleanup".into());
        }
        // Lifecycle owns containers/tasks. Never force-delete an unknown live
        // resource or its mounted snapshots just to make cleanup appear green.
        let containers = ContainersClient::new(self.channel.clone())
            .list(self.request(ListContainersRequest::default()))
            .await?
            .into_inner();
        let tasks = TasksClient::new(self.channel.clone())
            .list(self.request(ListTasksRequest::default()))
            .await?
            .into_inner();
        if !containers.containers.is_empty() || !tasks.tasks.is_empty() {
            return Err(format!(
                "live resources remain in owned namespace {}",
                self.namespace
            )
            .into());
        }
        let mut errors = Vec::new();
        let mut snapshots = self.snapshots().await?;
        // Delete leaves before their parents, with no broad snapshotter cleanup.
        while !snapshots.is_empty() {
            let parents: HashSet<_> = snapshots.iter().map(|info| info.parent.clone()).collect();
            let Some(index) = snapshots
                .iter()
                .position(|info| !parents.contains(&info.name))
            else {
                errors.push("snapshot parent graph has a cycle".to_string());
                break;
            };
            let snapshot = snapshots.swap_remove(index);
            if let Err(status) = SnapshotsClient::new(self.channel.clone())
                .remove(self.request(RemoveSnapshotRequest {
                    snapshotter: "overlayfs".to_string(),
                    key: snapshot.name,
                }))
                .await
                && status.code() != Code::NotFound
            {
                errors.push(format!("snapshot cleanup: {status}"));
            }
        }
        let mut images = ImagesClient::new(self.channel.clone());
        let listed = images
            .list(self.request(ListImagesRequest::default()))
            .await?
            .into_inner();
        for image in listed.images {
            // Sync waits for daemon GC after dropping this namespace's image
            // metadata. Never call DeleteContent or remove shared cache blobs.
            if let Err(status) = images
                .delete(self.request(DeleteImageRequest {
                    name: image.name,
                    sync: true,
                    target: image.target,
                }))
                .await
                && status.code() != Code::NotFound
            {
                errors.push(format!("image cleanup: {status}"));
            }
        }
        if !self.snapshots().await?.is_empty() {
            errors.push("snapshots remain after cleanup".to_string());
        }
        if !errors.is_empty() {
            return Err(errors.join("; ").into());
        }
        let mut request = Request::new(DeleteNamespaceRequest {
            name: self.namespace.clone(),
        });
        request.set_timeout(RPC_TIMEOUT);
        namespaces.delete(request).await?;
        // Namespace Get synthesizes an empty-label record even for a missing
        // namespace. Only List can prove that deletion actually removed it.
        let mut request = Request::new(ListNamespacesRequest::default());
        request.set_timeout(RPC_TIMEOUT);
        let remaining = namespaces.list(request).await?.into_inner().namespaces;
        if remaining
            .iter()
            .any(|namespace| namespace.name == self.namespace)
        {
            Err("owned namespace remains after cleanup".into())
        } else {
            Ok(())
        }
    }
}

fn reference() -> String {
    let image = support::image();
    let first = image.split('/').next().unwrap_or_default();
    if !image.contains('/') {
        format!("docker.io/library/{image}")
    } else if first.contains('.') || first.contains(':') || first == "localhost" {
        image
    } else {
        format!("docker.io/{image}")
    }
}

fn platform() -> support::Result<ImagePlatform> {
    // This integration test must actually execute the guest. Only the test
    // chooses a native default; the public preparation API has no host default.
    let native = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        architecture => architecture,
    };
    Ok(ImagePlatform::with_variant(
        std::env::var("CIRCUS_CONTAINERD_OS").unwrap_or_else(|_| "linux".into()),
        std::env::var("CIRCUS_CONTAINERD_ARCH").unwrap_or_else(|_| native.into()),
        std::env::var("CIRCUS_CONTAINERD_VARIANT").ok(),
    )?)
}

#[tokio::test]
#[ignore = "requires CIRCUS_CONTAINERD=1, a real Linux containerd daemon and anonymous registry access"]
async fn containerd_image_pull_alias_and_sandbox_execution() -> support::Result {
    support::enabled();
    let socket = support::socket();
    let reference = reference();
    let platform = platform()?;
    // Validate the local socket before connecting or creating any namespace.
    ContainerdImageRequest::new(
        socket.clone(),
        "circus-image-validation",
        reference.clone(),
        platform.clone(),
        Duration::from_secs(300),
    )?;
    let fixture = Fixture::create(&socket).await?;
    let mut sandbox = None;
    let outcome = AssertUnwindSafe(async {
        fixture.assert_image_absent(&reference).await?;
        let request = ContainerdImageRequest::new(
            socket.clone(),
            fixture.namespace.clone(),
            reference.clone(),
            platform.clone(),
            Duration::from_secs(300),
        )?;
        let missing = prepare_containerd_image(request.clone()).await;
        assert!(matches!(
            missing,
            Err(ContainerdImageError::ImageNotFound { reference: missing }) if missing == reference
        ));
        fixture.assert_image_absent(&reference).await?;

        let prepared =
            prepare_containerd_image(request.with_pull_policy(ImagePullPolicy::IfMissing)).await?;
        assert_eq!(prepared.platform().os(), platform.os());
        assert_eq!(prepared.platform().architecture(), platform.architecture());
        if let Some(variant) = platform.variant() {
            assert_eq!(prepared.platform().variant(), Some(variant));
        }
        let (_, qualified_digest) = prepared
            .reference()
            .split_once('@')
            .ok_or("alias is not immutable")?;
        assert_eq!(qualified_digest, prepared.digest());
        let hex = prepared
            .digest()
            .strip_prefix("sha256:")
            .ok_or("expected a SHA-256 manifest")?;
        assert_eq!(hex.len(), 64);
        assert!(hex.bytes().all(|byte| byte.is_ascii_hexdigit()));
        fixture
            .assert_selected_manifest(
                &reference,
                prepared.reference(),
                prepared.digest(),
                prepared.platform(),
            )
            .await?;
        let alias_before = fixture.image(prepared.reference()).await?;

        // Resolve the same cached source again: an index can carry variant
        // evidence that the selected manifest's config does not repeat.
        let repeated = prepare_containerd_image(ContainerdImageRequest::new(
            socket.clone(),
            fixture.namespace.clone(),
            reference.clone(),
            platform.clone(),
            Duration::from_secs(30),
        )?)
        .await?;
        assert_eq!(repeated.reference(), prepared.reference());
        assert_eq!(repeated.digest(), prepared.digest());
        assert_eq!(repeated.platform(), prepared.platform());
        // LocalOnly must reuse, not overwrite, the existing immutable record.
        assert_eq!(fixture.image(prepared.reference()).await?, alias_before);

        // Without index-only variant evidence, the immutable manifest alias
        // itself can also be prepared locally without changing its record.
        if prepared.platform().variant().is_none() {
            let from_alias = prepare_containerd_image(ContainerdImageRequest::new(
                socket.clone(),
                fixture.namespace.clone(),
                prepared.reference(),
                prepared.platform().clone(),
                Duration::from_secs(30),
            )?)
            .await?;
            assert_eq!(from_alias.reference(), prepared.reference());
            assert_eq!(from_alias.digest(), prepared.digest());
            assert_eq!(from_alias.platform(), prepared.platform());
            assert_eq!(fixture.image(prepared.reference()).await?, alias_before);
        }

        sandbox = Some(
            Sandbox::create(
                BackendConfig::containerd(
                    &socket,
                    &fixture.namespace,
                    prepared.reference(),
                    "runc",
                    "overlayfs",
                ),
                support::config()?,
            )
            .await?,
        );
        let guest = sandbox.as_ref().ok_or("sandbox missing")?;
        let id = guest
            .status()
            .await?
            .instance_id
            .ok_or("sandbox id missing")?;
        guest.start().await?;
        assert_eq!(guest.status().await?.phase, SandboxPhase::Running);
        let output = guest
            .exec(support::python(&[
                "-c",
                "print('prepared-containerd-image-ok')",
            ]))
            .await?;
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, b"prepared-containerd-image-ok\n");
        assert!(output.stderr.is_empty());
        guest.destroy().await?;
        assert_eq!(guest.status().await?.phase, SandboxPhase::Destroyed);
        fixture.assert_destroyed(&id).await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    })
    .catch_unwind()
    .await;

    // Retain the public sandbox handle across both errors and assertion panics,
    // so lifecycle cleanup runs before removing namespace-local image metadata.
    let mut cleanup_errors = Vec::new();
    if let Some(guest) = sandbox.as_ref()
        && let Err(error) = guest.destroy().await
    {
        cleanup_errors.push(format!("sandbox destroy: {error}"));
    }
    match tokio::time::timeout(Duration::from_secs(180), fixture.cleanup()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => cleanup_errors.push(format!("namespace {}: {error}", fixture.namespace)),
        Err(_) => cleanup_errors.push(format!("namespace {} cleanup timed out", fixture.namespace)),
    }
    match outcome {
        Err(panic) => {
            if !cleanup_errors.is_empty() {
                eprintln!("cleanup after failed test: {}", cleanup_errors.join("; "));
            }
            std::panic::resume_unwind(panic)
        }
        Ok(Err(error)) if cleanup_errors.is_empty() => Err(error),
        Ok(Err(error)) => Err(format!("{error}; cleanup: {}", cleanup_errors.join("; ")).into()),
        Ok(Ok(())) if !cleanup_errors.is_empty() => Err(cleanup_errors.join("; ").into()),
        Ok(Ok(())) => Ok(()),
    }
}
