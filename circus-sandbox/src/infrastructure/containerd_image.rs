//! Local image resolution and unpacking through containerd's content, diff,
//! image, and snapshot services.
//!
//! Nothing is pulled: every blob must already be in the content store. Unpack
//! is performed only when the image's committed chain snapshots are missing,
//! using the local layer descriptors and the configured snapshotter.

use std::collections::HashMap;

use containerd_client::services::v1::{
    CreateRequest as CreateLeaseRequest, DeleteRequest as DeleteLeaseRequest, GetImageRequest,
    Info as ContentInfo, ReadContentRequest, UpdateRequest as UpdateContentRequest,
    content_client::ContentClient,
    diff_client::DiffClient,
    images_client::ImagesClient,
    leases_client::LeasesClient,
    snapshots::{
        CommitSnapshotRequest, PrepareSnapshotRequest, RemoveSnapshotRequest, StatSnapshotRequest,
        snapshots_client::SnapshotsClient,
    },
};
use containerd_client::tonic::Request;
use containerd_client::tonic::transport::Channel;
use containerd_client::types::{Descriptor, Mount};
use containerd_client::with_namespace;

use super::containerd_grpc::map_status;
use super::containerd_spec::env_values;
use super::engine::{EngineError, EngineErrorKind, file_error};

const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;
/// gRPC metadata header containerd reads to attach requests to a lease.
const LEASE_HEADER: &str = "containerd-lease";
/// Label linking the image config content to its unpacked chain snapshot. It is
/// the same reference containerd's unpacker writes: without it a committed
/// chain is unreferenced and the garbage collector may reclaim it while the
/// daemon is unpacking later layers or the caller is still preparing a child
/// snapshot.
const SNAPSHOT_REF_LABEL_PREFIX: &str = "containerd.io/gc.ref.snapshot.";
pub(super) const INDEX_MEDIA_TYPES: [&str; 2] = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];

/// One layer descriptor together with its containerd chain identity.
pub(crate) struct ChainLayer {
    pub chain_id: String,
    pub descriptor: Descriptor,
}

pub(crate) struct ImageRootfs {
    pub chain_id: String,
    pub layers: Vec<ChainLayer>,
    pub config: Descriptor,
    pub env: Vec<String>,
}

// One unpack at a time inside this process: two sandboxes created
// concurrently from the same unpacked image must not apply a layer twice.
static UNPACK_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Verifies that an image record exists in the namespace without pulling.
///
/// Familiar short references are resolved the way containerd's own clients do:
/// `python:3.12-slim` is looked up as `docker.io/library/python:3.12-slim`
/// after the literal name. Nothing is normalized on the daemon.
pub(crate) async fn image_exists(
    channel: &Channel,
    namespace: &str,
    image: &str,
) -> Result<(), EngineError> {
    get_image(channel, namespace, image).await.map(|_| ())
}

pub(super) async fn get_image(
    channel: &Channel,
    namespace: &str,
    image: &str,
) -> Result<containerd_client::services::v1::Image, EngineError> {
    let mut client = ImagesClient::new(channel.clone());
    let mut last = None;
    for candidate in image_candidates(image) {
        let request = with_namespace!(GetImageRequest { name: candidate }, namespace);
        match client.get(request).await {
            Ok(response) => {
                return response.into_inner().image.ok_or_else(|| {
                    file_error(EngineErrorKind::Other, "image record has no target")
                });
            }
            Err(status) if status.code() == containerd_client::tonic::Code::NotFound => {
                last = Some(map_status(status));
            }
            Err(status) => return Err(map_status(status)),
        }
    }
    Err(last.unwrap_or_else(|| file_error(EngineErrorKind::NotFound, "image reference is empty")))
}

/// Candidate containerd names for one requested reference.
pub(crate) fn image_candidates(image: &str) -> Vec<String> {
    let mut candidates = vec![image.to_string()];
    let mut parts = image.splitn(2, '/');
    let first = parts.next().unwrap_or_default();
    let rest = parts.next();
    let normalized = match rest {
        None => format!("docker.io/library/{image}"),
        Some(_) if first.contains('.') || first.contains(':') || first == "localhost" => {
            return candidates;
        }
        Some(_) => format!("docker.io/{image}"),
    };
    candidates.push(normalized);
    candidates
}

/// Resolves the image's rootfs chain and ensures every committed snapshot
/// exists, unpacking local layers only when they do not.
pub(crate) async fn ensure_rootfs(
    channel: &Channel,
    namespace: &str,
    snapshotter: &str,
    image: &str,
) -> Result<(String, Vec<String>), EngineError> {
    let _guard = UNPACK_LOCK.lock().await;
    let rootfs = resolve_rootfs(channel, namespace, image).await?;
    if snapshot_committed(channel, namespace, snapshotter, &rootfs.chain_id).await? {
        return Ok((rootfs.chain_id, rootfs.env));
    }
    // containerd collects every resource that is neither referenced nor leased.
    // The extraction snapshots only become referenced by the image config at
    // the end, so a concurrent GC can reclaim them mid-apply without a lease.
    let lease = create_unpack_lease(channel, namespace).await?;
    let unpacked = unpack_layers(channel, namespace, snapshotter, &rootfs, &lease).await;
    delete_lease_best_effort(channel, namespace, &lease).await;
    unpacked.map(|()| (rootfs.chain_id, rootfs.env))
}

async fn unpack_layers(
    channel: &Channel,
    namespace: &str,
    snapshotter: &str,
    rootfs: &ImageRootfs,
    lease: &str,
) -> Result<(), EngineError> {
    let mut parent = String::new();
    for layer in &rootfs.layers {
        if !snapshot_committed(channel, namespace, snapshotter, &layer.chain_id).await? {
            unpack_layer(
                channel,
                namespace,
                snapshotter,
                &layer.chain_id,
                &parent,
                &layer.descriptor,
                lease,
            )
            .await?;
        }
        parent = layer.chain_id.clone();
    }
    if !snapshot_committed(channel, namespace, snapshotter, &rootfs.chain_id).await? {
        return Err(file_error(
            EngineErrorKind::Other,
            "image unpack completed without the final committed snapshot",
        ));
    }
    // Keep the committed chain reachable from the image config, exactly as
    // containerd's unpacker does, before the lease is dropped.
    reference_chain_from_config(channel, namespace, snapshotter, rootfs).await
}

async fn resolve_rootfs(
    channel: &Channel,
    namespace: &str,
    image: &str,
) -> Result<ImageRootfs, EngineError> {
    let image = get_image(channel, namespace, image).await?;
    let target = image.target.ok_or_else(|| {
        file_error(
            EngineErrorKind::Other,
            "image record has no target descriptor",
        )
    })?;
    let manifest = if INDEX_MEDIA_TYPES.contains(&target.media_type.as_str()) {
        index_manifest(channel, namespace, &target).await?
    } else {
        target
    };
    let manifest_json = read_content(channel, namespace, &manifest).await?;
    let parsed = parse_manifest(&manifest_json)?;
    let config_json = read_content(channel, namespace, &parsed.config).await?;
    let (diff_ids, env) = parse_config(&config_json)?;
    if diff_ids.is_empty() {
        return Err(file_error(
            EngineErrorKind::Other,
            "image config has no rootfs layers",
        ));
    }
    if diff_ids.len() != parsed.layers.len() {
        return Err(file_error(
            EngineErrorKind::Other,
            "image config diff_ids and manifest layers disagree",
        ));
    }
    let chain = chain_ids(&diff_ids);
    let layers = chain
        .into_iter()
        .zip(parsed.layers)
        .map(|(chain_id, descriptor)| ChainLayer {
            chain_id,
            descriptor,
        })
        .collect();
    Ok(ImageRootfs {
        chain_id: chain_id_for(&diff_ids),
        layers,
        config: parsed.config,
        env,
    })
}

async fn index_manifest(
    channel: &Channel,
    namespace: &str,
    target: &Descriptor,
) -> Result<Descriptor, EngineError> {
    let index = read_content(channel, namespace, target).await?;
    let value: serde_json::Value = serde_json::from_slice(&index).map_err(|err| {
        file_error(
            EngineErrorKind::Other,
            format!("image index is not JSON: {err}"),
        )
    })?;
    let manifests = value
        .get("manifests")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| file_error(EngineErrorKind::Other, "image index has no manifests"))?;
    let host = host_architecture();
    let selected = manifests
        .iter()
        .find(|entry| platform_matches(entry, "linux", host))
        .ok_or_else(|| {
            file_error(
                EngineErrorKind::Other,
                format!("image index has no linux/{host} manifest"),
            )
        })?;
    descriptor_from_value(selected)
}

fn platform_matches(entry: &serde_json::Value, os: &str, architecture: &str) -> bool {
    let platform = entry.get("platform");
    let matches = |field: &str, expected: &str| {
        platform
            .and_then(|platform| platform.get(field))
            .and_then(serde_json::Value::as_str)
            == Some(expected)
    };
    matches("os", os) && matches("architecture", architecture)
}

pub(super) fn descriptor_from_value(value: &serde_json::Value) -> Result<Descriptor, EngineError> {
    let media_type = value
        .get("mediaType")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| file_error(EngineErrorKind::Other, "descriptor has no mediaType"))?;
    let digest = value
        .get("digest")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| file_error(EngineErrorKind::Other, "descriptor has no digest"))?;
    let size = value
        .get("size")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_default();
    Ok(Descriptor {
        media_type: media_type.to_string(),
        digest: digest.to_string(),
        size,
        annotations: HashMap::new(),
    })
}

pub(super) struct Manifest {
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

pub(super) fn parse_manifest(bytes: &[u8]) -> Result<Manifest, EngineError> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|err| {
        file_error(
            EngineErrorKind::Other,
            format!("image manifest is not JSON: {err}"),
        )
    })?;
    let config = value
        .get("config")
        .ok_or_else(|| file_error(EngineErrorKind::Other, "manifest has no config"))?;
    let layers = value
        .get("layers")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| file_error(EngineErrorKind::Other, "manifest has no layers"))?;
    Ok(Manifest {
        config: descriptor_from_value(config)?,
        layers: layers
            .iter()
            .map(descriptor_from_value)
            .collect::<Result<_, _>>()?,
    })
}

fn parse_config(bytes: &[u8]) -> Result<(Vec<String>, Vec<String>), EngineError> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|err| {
        file_error(
            EngineErrorKind::Other,
            format!("image config is not JSON: {err}"),
        )
    })?;
    let settings = match value.get("config") {
        None | Some(serde_json::Value::Null) => None,
        Some(settings) if settings.is_object() => Some(settings),
        Some(_) => {
            return Err(file_error(
                EngineErrorKind::Other,
                "image config settings are not an object",
            ));
        }
    };
    let env = match settings.and_then(|settings| settings.get("Env")) {
        None | Some(serde_json::Value::Null) => &[][..],
        Some(env) => env_values(env)?,
    };
    let ids = value
        .pointer("/rootfs/diff_ids")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| file_error(EngineErrorKind::Other, "image config has no diff_ids"))?;
    let diff_ids = ids
        .iter()
        .map(|id| {
            id.as_str()
                .map(str::to_string)
                .ok_or_else(|| file_error(EngineErrorKind::Other, "image diff_id is not a string"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let env = env
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_string)
        .collect();
    Ok((diff_ids, env))
}

/// containerd chain identity: each step hashes the previous chain id and the
/// next diff id; the final chain id names the committed rootfs snapshot.
fn chain_ids(diff_ids: &[String]) -> Vec<String> {
    let mut chain: Vec<String> = Vec::with_capacity(diff_ids.len());
    for diff_id in diff_ids {
        match chain.last() {
            None => chain.push(diff_id.clone()),
            Some(previous) => chain.push(sha256_hex(format!("{previous} {diff_id}"))),
        }
    }
    chain
}

fn chain_id_for(diff_ids: &[String]) -> String {
    chain_ids(diff_ids).pop().unwrap_or_default()
}

fn sha256_hex(input: String) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{}", hex(&Sha256::digest(input.as_bytes())))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

fn host_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "x86" => "386",
        "aarch64" => "arm64",
        "arm" => "arm",
        "powerpc64" => "ppc64le",
        "s390x" => "s390x",
        "riscv64" => "riscv64",
        other => other,
    }
}

pub(super) async fn read_content(
    channel: &Channel,
    namespace: &str,
    descriptor: &Descriptor,
) -> Result<Vec<u8>, EngineError> {
    let mut client = ContentClient::new(channel.clone());
    let request = with_namespace!(
        ReadContentRequest {
            digest: descriptor.digest.clone(),
            offset: 0,
            size: descriptor.size,
        },
        namespace
    );
    let mut stream = client.read(request).await.map_err(map_status)?.into_inner();
    let mut bytes = Vec::new();
    while let Some(message) = stream.message().await.map_err(map_status)? {
        if bytes.len().saturating_add(message.data.len()) > MAX_METADATA_BYTES {
            return Err(file_error(
                EngineErrorKind::Limit,
                "image metadata exceeds the 8 MiB bound",
            ));
        }
        bytes.extend_from_slice(&message.data);
    }
    Ok(bytes)
}

async fn snapshot_committed(
    channel: &Channel,
    namespace: &str,
    snapshotter: &str,
    key: &str,
) -> Result<bool, EngineError> {
    let mut client = SnapshotsClient::new(channel.clone());
    let request = with_namespace!(
        StatSnapshotRequest {
            snapshotter: snapshotter.to_string(),
            key: key.to_string(),
        },
        namespace
    );
    match client.stat(request).await {
        Ok(_) => Ok(true),
        Err(status) if status.code() == containerd_client::tonic::Code::NotFound => Ok(false),
        Err(status) => Err(map_status(status)),
    }
}

async fn unpack_layer(
    channel: &Channel,
    namespace: &str,
    snapshotter: &str,
    chain_id: &str,
    parent: &str,
    descriptor: &Descriptor,
    lease: &str,
) -> Result<(), EngineError> {
    // containerd commits the extraction snapshot under the chain id, so the
    // active snapshot needs a distinct key. Sharing one string for both makes
    // Commit fail with AlreadyExists after the layer has already been applied.
    let key = format!("extract-{}", uuid::Uuid::new_v4());
    let mounts = prepare_snapshot(channel, namespace, snapshotter, &key, parent, lease).await?;
    if let Err(error) = apply_diff(channel, namespace, descriptor, &mounts).await {
        remove_snapshot_best_effort(channel, namespace, snapshotter, &key).await;
        return Err(error);
    }
    let mut client = SnapshotsClient::new(channel.clone());
    let request = with_lease(
        with_namespace!(
            CommitSnapshotRequest {
                snapshotter: snapshotter.to_string(),
                name: chain_id.to_string(),
                key: key.clone(),
                // The snapshot references its layer content so the blob is
                // retained under containerd's garbage collector while the
                // snapshot exists.
                labels: HashMap::from([(
                    "containerd.io/gc.ref.content.0".to_string(),
                    descriptor.digest.clone(),
                )]),
            },
            namespace
        ),
        lease,
    );
    match client.commit(request).await {
        Ok(_) => Ok(()),
        Err(status) if status.code() == containerd_client::tonic::Code::AlreadyExists => {
            // A concurrent unpacker committed this chain first. That committed
            // chain is the resource we need; discard only our extraction
            // snapshot.
            remove_snapshot_best_effort(channel, namespace, snapshotter, &key).await;
            if snapshot_committed(channel, namespace, snapshotter, chain_id).await? {
                Ok(())
            } else {
                Err(map_status(status))
            }
        }
        Err(status) => {
            remove_snapshot_best_effort(channel, namespace, snapshotter, &key).await;
            Err(map_status(status))
        }
    }
}

async fn prepare_snapshot(
    channel: &Channel,
    namespace: &str,
    snapshotter: &str,
    key: &str,
    parent: &str,
    lease: &str,
) -> Result<Vec<Mount>, EngineError> {
    let mut client = SnapshotsClient::new(channel.clone());
    let request = with_lease(
        with_namespace!(
            PrepareSnapshotRequest {
                snapshotter: snapshotter.to_string(),
                key: key.to_string(),
                parent: parent.to_string(),
                labels: HashMap::new(),
            },
            namespace
        ),
        lease,
    );
    Ok(client
        .prepare(request)
        .await
        .map_err(map_status)?
        .into_inner()
        .mounts)
}

async fn create_unpack_lease(channel: &Channel, namespace: &str) -> Result<String, EngineError> {
    let id = format!("circus-unpack-{}", uuid::Uuid::new_v4());
    let request = with_namespace!(
        CreateLeaseRequest {
            id: id.clone(),
            labels: HashMap::new(),
        },
        namespace
    );
    LeasesClient::new(channel.clone())
        .create(request)
        .await
        .map_err(map_status)?;
    Ok(id)
}

async fn delete_lease_best_effort(channel: &Channel, namespace: &str, id: &str) {
    let request = with_namespace!(
        DeleteLeaseRequest {
            id: id.to_string(),
            sync: false,
        },
        namespace
    );
    let _ = LeasesClient::new(channel.clone()).delete(request).await;
}

/// Marks the image config as referencing the unpacked chain, the reference
/// containerd's unpacker records so the chain survives garbage collection for
/// as long as the image exists.
async fn reference_chain_from_config(
    channel: &Channel,
    namespace: &str,
    snapshotter: &str,
    rootfs: &ImageRootfs,
) -> Result<(), EngineError> {
    let label = format!("{SNAPSHOT_REF_LABEL_PREFIX}{snapshotter}");
    let request = with_namespace!(
        UpdateContentRequest {
            info: Some(ContentInfo {
                digest: rootfs.config.digest.clone(),
                size: rootfs.config.size,
                created_at: None,
                updated_at: None,
                labels: HashMap::from([(label.clone(), rootfs.chain_id.clone())]),
            }),
            update_mask: Some(prost_types::FieldMask {
                paths: vec![format!("labels.{label}")],
            }),
        },
        namespace
    );
    ContentClient::new(channel.clone())
        .update(request)
        .await
        .map_err(map_status)?;
    Ok(())
}

fn with_lease<T>(mut request: Request<T>, lease: &str) -> Request<T> {
    request.metadata_mut().insert(
        LEASE_HEADER,
        lease.parse().expect("lease id is a valid header value"),
    );
    request
}

async fn apply_diff(
    channel: &Channel,
    namespace: &str,
    descriptor: &Descriptor,
    mounts: &[Mount],
) -> Result<(), EngineError> {
    let mut client = DiffClient::new(channel.clone());
    let request = with_namespace!(
        containerd_client::services::v1::ApplyRequest {
            diff: Some(descriptor.clone()),
            mounts: mounts.to_vec(),
            payloads: HashMap::new(),
            sync_fs: false,
        },
        namespace
    );
    client.apply(request).await.map_err(map_status)?;
    Ok(())
}

async fn remove_snapshot_best_effort(
    channel: &Channel,
    namespace: &str,
    snapshotter: &str,
    key: &str,
) {
    let mut client = SnapshotsClient::new(channel.clone());
    let request = with_namespace!(
        RemoveSnapshotRequest {
            snapshotter: snapshotter.to_string(),
            key: key.to_string(),
        },
        namespace
    );
    let _ = client.remove(request).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known containerd chain ids for python:3.12-slim linux/amd64, read from
    // the live daemon's committed snapshots. The algorithm must match exactly.
    const DIFF_IDS: [&str; 4] = [
        "sha256:a70f6272827f64e1427d048ced77491aab5d64ab95725e56920802ff391197d7",
        "sha256:33d3a946838548b1885b1068b960a88e41537b6996342c8fc2c767a6879e62f4",
        "sha256:500ecb68a701ad0437ae759f35178f2ddbc48e10a0bc6dc345548054f737df6a",
        "sha256:93881a54803b57e0039f40fd78de98b41b69b55d025c431544788fd84f0904cd",
    ];

    #[test]
    fn chain_ids_match_containerd_unpack() {
        let diff_ids = DIFF_IDS.map(str::to_string).to_vec();
        let chain = chain_ids(&diff_ids);
        assert_eq!(chain[0], DIFF_IDS[0]);
        assert_eq!(
            chain[1],
            "sha256:af995e0458453007a40b917922dc59b8834ba36fb1564fa85ba6f9d6795e8629"
        );
        assert_eq!(
            chain[2],
            "sha256:24eb450e9d3b775f6a1200544ef8f5da86b0d3af323e04a803f011cb246c9915"
        );
        assert_eq!(
            chain[3],
            "sha256:241d372eec8f05a802bb5ca99fba52dd93480b904cb0ca4f591e4f2e27ffd6da"
        );
    }

    #[test]
    fn sha256_hex_is_lowercase_64_digits() {
        let digest = sha256_hex("abc".to_string());
        assert_eq!(
            digest,
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn unpack_requests_carry_the_lease_header() {
        let request = with_lease(Request::new(()), "circus-unpack-test");
        assert_eq!(
            request
                .metadata()
                .get(LEASE_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("circus-unpack-test")
        );
    }

    #[test]
    fn manifest_parsing_keeps_layer_order() {
        let manifest = br#"{
            "config": {"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:a","size":2},
            "layers": [
                {"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"sha256:b","size":3},
                {"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"sha256:c","size":4}
            ]
        }"#;
        let parsed = parse_manifest(manifest).unwrap();
        assert_eq!(parsed.config.digest, "sha256:a");
        assert_eq!(parsed.layers.len(), 2);
        assert_eq!(parsed.layers[1].digest, "sha256:c");
        assert_eq!(parsed.layers[1].size, 4);
    }

    #[test]
    fn config_parsing_preserves_image_environment_and_rootfs_together() {
        let (diff_ids, env) = parse_config(
            br#"{
                "rootfs":{"diff_ids":["sha256:a","sha256:b"]},
                "config":{
                    "Env":["HOME=/workspace","CODEX_HOME=/workspace/codex","LANG=","A=b=c"],
                    "Entrypoint":["not-the-init"],
                    "User":"not-the-lifecycle-user",
                    "WorkingDir":"/not-the-workspace"
                }
            }"#,
        )
        .unwrap();
        assert_eq!(diff_ids, ["sha256:a", "sha256:b"]);
        assert_eq!(
            env,
            [
                "HOME=/workspace",
                "CODEX_HOME=/workspace/codex",
                "LANG=",
                "A=b=c"
            ]
        );
    }

    #[test]
    fn optional_image_environment_can_be_absent_or_empty() {
        for config in [
            serde_json::json!({"rootfs":{"diff_ids":["sha256:a"]}}),
            serde_json::json!({"rootfs":{"diff_ids":["sha256:a"]},"config":null}),
            serde_json::json!({"rootfs":{"diff_ids":["sha256:a"]},"config":{}}),
            serde_json::json!({"rootfs":{"diff_ids":["sha256:a"]},"config":{"Env":null}}),
            serde_json::json!({"rootfs":{"diff_ids":["sha256:a"]},"config":{"Env":[]}}),
        ] {
            let (_, env) = parse_config(&serde_json::to_vec(&config).unwrap()).unwrap();
            assert!(env.is_empty());
        }
    }

    #[test]
    fn malformed_image_environment_is_not_treated_as_missing() {
        for settings in [
            serde_json::json!([]),
            serde_json::json!("invalid-settings"),
            serde_json::json!({"Env":{}}),
            serde_json::json!({"Env":"HOME=/workspace"}),
            serde_json::json!({"Env":[null]}),
            serde_json::json!({"Env":[3]}),
            serde_json::json!({"Env":["HOME"]}),
            serde_json::json!({"Env":["=empty-key"]}),
            serde_json::json!({"Env":["HOME=/bad\0value"]}),
            serde_json::json!({"Env":["BAD\0KEY=value"]}),
        ] {
            let config = serde_json::json!({
                "rootfs":{"diff_ids":["sha256:a"]},
                "config":settings,
            });
            assert!(parse_config(&serde_json::to_vec(&config).unwrap()).is_err());
        }
    }

    #[test]
    fn diff_ids_require_rootfs_array() {
        assert!(parse_config(br#"{"rootfs":{"diff_ids":["sha256:a"]}}"#).is_ok());
        assert!(parse_config(br#"{"rootfs":{}}"#).is_err());
        assert!(parse_config(b"not json").is_err());
    }

    #[test]
    fn familiar_references_are_looked_up_at_docker_hub() {
        assert_eq!(
            image_candidates("python:3.12-slim"),
            vec![
                "python:3.12-slim".to_string(),
                "docker.io/library/python:3.12-slim".to_string(),
            ]
        );
        assert_eq!(
            image_candidates("library/python:3.12-slim"),
            vec![
                "library/python:3.12-slim".to_string(),
                "docker.io/library/python:3.12-slim".to_string(),
            ]
        );
        assert_eq!(
            image_candidates("docker.io/library/python:3.12-slim"),
            vec!["docker.io/library/python:3.12-slim".to_string()]
        );
        assert_eq!(
            image_candidates("localhost:5000/app:1"),
            vec!["localhost:5000/app:1".to_string()]
        );
    }
}
