//! Deterministic public-API policy tests against a Unix-socket gRPC peer.
//! This verifies client protocol and policy, not a real daemon or isolation.
#![cfg(all(unix, feature = "containerd"))]

#[path = "containerd_image_support/mod.rs"]
mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use circus_sandbox::environment::{
    ContainerdImageError, ContainerdImageRequest, ImagePlatform, ImagePullPolicy,
    prepare_containerd_image,
};
use containerd_client::services::v1::Image;
use containerd_client::tonic::Code;
use containerd_client::types::{
    Descriptor,
    transfer::{ImageStore, OciRegistry},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tonic_prost::prost::Message;

use support::{Event, Hang, NAMESPACE, Peer, REFERENCE, State};

const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX: &str = "application/vnd.oci.image.index.v1+json";
const CONFIG: &str = "application/vnd.oci.image.config.v1+json";

fn platform() -> ImagePlatform {
    ImagePlatform::with_variant("linux", "arm64", Some("v8".into())).unwrap()
}

fn request(peer: &Peer, policy: ImagePullPolicy, timeout: Duration) -> ContainerdImageRequest {
    ContainerdImageRequest::new(&peer.socket, NAMESPACE, REFERENCE, platform(), timeout)
        .unwrap()
        .with_pull_policy(policy)
}

fn descriptor(bytes: &[u8], media_type: &str) -> Descriptor {
    Descriptor {
        media_type: media_type.into(),
        digest: format!("sha256:{:x}", Sha256::digest(bytes)),
        size: i64::try_from(bytes.len()).unwrap(),
        ..Default::default()
    }
}

struct Fixture {
    state: State,
    index: Descriptor,
    manifest: Descriptor,
    config: Descriptor,
}

fn fixture(actual_arch: &str, actual_variant: Option<&str>) -> Fixture {
    let mut config = json!({"os":"linux", "architecture": actual_arch,
        "rootfs":{"type":"layers", "diff_ids":[]}, "config":{}});
    if let Some(variant) = actual_variant {
        config["variant"] = json!(variant);
    }
    fixture_with_config(config)
}

fn fixture_with_config(config: serde_json::Value) -> Fixture {
    let config_bytes = serde_json::to_vec(&config).unwrap();
    let config = descriptor(&config_bytes, CONFIG);
    let manifest_bytes = serde_json::to_vec(&json!({
        "schemaVersion":2, "mediaType":MANIFEST,
        "config":{"mediaType":CONFIG, "digest":config.digest, "size":config.size},
        "layers":[]
    }))
    .unwrap();
    let manifest = descriptor(&manifest_bytes, MANIFEST);
    // Put a different platform first. Only the requested platform's blobs exist,
    // so choosing the first entry or silently using the host cannot succeed.
    let index_bytes = serde_json::to_vec(&json!({
        "schemaVersion":2, "mediaType":INDEX,
        "manifests":[
            {"mediaType":MANIFEST, "digest":format!("sha256:{}", "a".repeat(64)),
                "size":123, "platform":{"os":"linux", "architecture":"amd64"}},
            {"mediaType":MANIFEST, "digest":manifest.digest, "size":manifest.size,
                "platform":{"os":"linux", "architecture":"arm64", "variant":"v8"}}
        ]
    }))
    .unwrap();
    let index = descriptor(&index_bytes, INDEX);
    let blobs = BTreeMap::from([
        (index.digest.clone(), index_bytes),
        (manifest.digest.clone(), manifest_bytes),
        (config.digest.clone(), config_bytes),
    ]);
    Fixture {
        state: State {
            image: Image {
                name: REFERENCE.into(),
                target: Some(index.clone()),
                ..Default::default()
            },
            blobs,
            missing: false,
            lookup_error: None,
            existing_alias: None,
            hang: None,
            transfer_gate: None,
            events: Vec::new(),
            aliases: BTreeMap::new(),
        },
        index,
        manifest,
        config,
    }
}

fn assert_no_transfer(events: &[Event]) {
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Transfer(_)))
    );
}

fn assert_no_alias(peer: &Peer) {
    let state = peer.state.lock().unwrap();
    assert!(state.aliases.is_empty());
    assert!(
        !state
            .events
            .iter()
            .any(|event| matches!(event, Event::Create(_)))
    );
}

#[tokio::test]
async fn missing_local_only_never_transfers_or_reads() {
    let mut data = fixture("arm64", Some("v8"));
    data.state.missing = true;
    let peer = Peer::start(data.state);
    let error = prepare_containerd_image(request(
        &peer,
        ImagePullPolicy::LocalOnly,
        Duration::from_secs(5),
    ))
    .await
    .unwrap_err();
    assert_eq!(
        error,
        ContainerdImageError::ImageNotFound {
            reference: REFERENCE.into()
        }
    );
    assert_eq!(peer.events(), vec![Event::Get(REFERENCE.into())]);
    assert_no_alias(&peer);
}

#[tokio::test]
async fn actual_miss_transfers_explicit_platform_then_pins_selected_manifest() {
    let mut data = fixture("arm64", Some("v8"));
    data.state.missing = true;
    let peer = Peer::start(data.state);
    let prepared = prepare_containerd_image(request(
        &peer,
        ImagePullPolicy::IfMissing,
        Duration::from_secs(5),
    ))
    .await
    .unwrap();
    let alias = format!("registry.example/team/image@{}", data.manifest.digest);
    assert_eq!(prepared.reference(), alias);
    assert_eq!(prepared.digest(), data.manifest.digest);
    assert_ne!(prepared.digest(), data.index.digest);
    assert_eq!(prepared.platform(), &platform());

    let events = peer.events();
    assert_eq!(events.len(), 7);
    assert_eq!(events[0], Event::Get(REFERENCE.into()));
    let Event::Transfer(transfer) = &events[1] else {
        panic!("expected transfer after Get NotFound")
    };
    let source = transfer.source.as_ref().unwrap();
    assert!(
        source
            .type_url
            .ends_with("containerd.types.transfer.OCIRegistry")
    );
    let registry = OciRegistry::decode(source.value.as_slice()).unwrap();
    assert_eq!(registry.reference, REFERENCE);
    assert!(
        registry.resolver.is_none(),
        "no auth stream, host config, or headers"
    );
    let destination = transfer.destination.as_ref().unwrap();
    assert!(
        destination
            .type_url
            .ends_with("containerd.types.transfer.ImageStore")
    );
    let store = ImageStore::decode(destination.value.as_slice()).unwrap();
    assert_eq!(store.name, REFERENCE);
    assert!(store.unpacks.is_empty());
    assert_eq!(store.platforms.len(), 1);
    assert_eq!(store.platforms[0].os, "linux");
    assert_eq!(store.platforms[0].architecture, "arm64");
    assert_eq!(store.platforms[0].variant, "v8");
    assert_eq!(events[2], Event::Get(REFERENCE.into()));
    assert_eq!(events[3], Event::Read(data.index.digest));
    assert_eq!(events[4], Event::Read(data.manifest.digest.clone()));
    assert_eq!(events[5], Event::Read(data.config.digest));
    let Event::Create(image) = &events[6] else {
        panic!("expected immutable alias creation")
    };
    assert_eq!(image.name, alias);
    assert_eq!(image.target.as_ref(), Some(&data.manifest));
    assert_eq!(
        peer.state.lock().unwrap().aliases.get(&alias),
        Some(&data.manifest)
    );
}

#[tokio::test]
async fn dishonest_index_cannot_override_actual_config_platform() {
    for (arch, variant) in [("amd64", None), ("arm64", Some("v9"))] {
        let data = fixture(arch, variant);
        let peer = Peer::start(data.state);
        let error = prepare_containerd_image(request(
            &peer,
            ImagePullPolicy::IfMissing,
            Duration::from_secs(5),
        ))
        .await
        .unwrap_err();
        assert_eq!(
            error,
            ContainerdImageError::PlatformMismatch {
                expected: Box::new(platform()),
                actual: Some(
                    ImagePlatform::with_variant("linux", arch, variant.map(str::to_owned)).unwrap()
                ),
            }
        );
        assert_eq!(
            peer.events(),
            vec![
                Event::Get(REFERENCE.into()),
                Event::Read(data.index.digest),
                Event::Read(data.manifest.digest),
                Event::Read(data.config.digest),
            ]
        );
        assert_no_transfer(&peer.events());
        assert_no_alias(&peer);
    }
}

#[tokio::test]
async fn cached_index_without_exact_variant_never_transfers() {
    let data = fixture("arm64", Some("v8"));
    let peer = Peer::start(data.state);
    let expected = ImagePlatform::with_variant("linux", "arm64", Some("v9".into())).unwrap();
    let request = ContainerdImageRequest::new(
        &peer.socket,
        NAMESPACE,
        REFERENCE,
        expected.clone(),
        Duration::from_secs(5),
    )
    .unwrap()
    .with_pull_policy(ImagePullPolicy::IfMissing);
    let error = prepare_containerd_image(request).await.unwrap_err();
    assert_eq!(
        error,
        ContainerdImageError::PlatformMismatch {
            expected: Box::new(expected),
            actual: None,
        }
    );
    assert_eq!(
        peer.events(),
        vec![Event::Get(REFERENCE.into()), Event::Read(data.index.digest),]
    );
    assert_no_transfer(&peer.events());
    assert_no_alias(&peer);
}

#[tokio::test]
async fn lookup_permission_and_unknown_failures_never_authorize_transfer() {
    for code in [Code::PermissionDenied, Code::Unknown] {
        let mut data = fixture("arm64", Some("v8"));
        data.state.lookup_error = Some(code);
        let peer = Peer::start(data.state);
        let error = prepare_containerd_image(request(
            &peer,
            ImagePullPolicy::IfMissing,
            Duration::from_secs(5),
        ))
        .await
        .unwrap_err();
        let ContainerdImageError::Daemon { message } = error else {
            panic!("expected daemon error")
        };
        assert!(!message.contains("secret"));
        assert!(!message.contains("credential"));
        assert_eq!(peer.events(), vec![Event::Get(REFERENCE.into())]);
        assert_no_alias(&peer);
    }
}

#[tokio::test]
async fn malformed_cached_metadata_never_triggers_transfer() {
    let mut data = fixture("arm64", Some("v8"));
    let bytes = b"{not valid JSON".to_vec();
    let malformed = descriptor(&bytes, INDEX);
    data.state.image.target = Some(malformed.clone());
    data.state.blobs.insert(malformed.digest.clone(), bytes);
    let peer = Peer::start(data.state);
    let error = prepare_containerd_image(request(
        &peer,
        ImagePullPolicy::IfMissing,
        Duration::from_secs(5),
    ))
    .await
    .unwrap_err();
    assert!(matches!(error, ContainerdImageError::Daemon { .. }));
    assert_eq!(
        peer.events(),
        vec![Event::Get(REFERENCE.into()), Event::Read(malformed.digest)]
    );
    assert_no_alias(&peer);
}

#[tokio::test]
async fn oversized_streamed_metadata_is_bounded_without_transfer() {
    let mut data = fixture("arm64", Some("v8"));
    // Valid JSON beyond the metadata bound, split into sub-4-MiB gRPC
    // messages by the peer. Failure must not just be a codec message limit.
    let mut bytes = vec![b' '; 8 * 1024 * 1024];
    bytes.extend_from_slice(data.state.blobs.get(&data.index.digest).unwrap());
    let oversized = descriptor(&bytes, INDEX);
    data.state.image.target = Some(oversized.clone());
    data.state.blobs.insert(oversized.digest.clone(), bytes);
    let peer = Peer::start(data.state);
    let error = prepare_containerd_image(request(
        &peer,
        ImagePullPolicy::IfMissing,
        Duration::from_secs(5),
    ))
    .await
    .unwrap_err();
    assert!(matches!(error, ContainerdImageError::Daemon { .. }));
    assert_eq!(
        peer.events(),
        vec![Event::Get(REFERENCE.into()), Event::Read(oversized.digest)]
    );
    assert_no_alias(&peer);
}

#[tokio::test]
async fn existing_alias_uses_stable_identity_and_conflict_is_never_overwritten() {
    for (conflict, different_annotations) in [(false, false), (false, true), (true, false)] {
        let mut data = fixture("arm64", Some("v8"));
        let mut existing = data.manifest.clone();
        if conflict {
            existing.digest = format!("sha256:{}", "b".repeat(64));
        }
        if different_annotations {
            existing
                .annotations
                .insert("example.com/alias-note".into(), "different".into());
        }
        data.state.existing_alias = Some(existing.clone());
        let peer = Peer::start(data.state);
        let result = prepare_containerd_image(request(
            &peer,
            ImagePullPolicy::IfMissing,
            Duration::from_secs(5),
        ))
        .await;
        let alias = format!("registry.example/team/image@{}", data.manifest.digest);
        if conflict {
            assert_eq!(
                result.unwrap_err(),
                ContainerdImageError::AliasConflict {
                    reference: alias.clone()
                }
            );
        } else {
            assert_eq!(result.unwrap().reference(), alias);
        }
        assert_eq!(
            peer.events(),
            vec![
                Event::Get(REFERENCE.into()),
                Event::Read(data.index.digest),
                Event::Read(data.manifest.digest.clone()),
                Event::Read(data.config.digest),
                Event::Create(Image {
                    name: alias.clone(),
                    target: Some(data.manifest),
                    ..Default::default()
                }),
                Event::Get(alias),
            ]
        );
        let state = peer.state.lock().unwrap();
        assert_eq!(state.existing_alias.as_ref(), Some(&existing));
        assert!(state.aliases.is_empty());
    }
}

#[tokio::test]
async fn whole_deadline_covers_transfer_streaming_read_and_alias_creation() {
    for phase in [Hang::Transfer, Hang::Read, Hang::Alias] {
        let mut data = fixture("arm64", Some("v8"));
        data.state.hang = Some(phase);
        data.state.missing = phase == Hang::Transfer;
        let peer = Peer::start(data.state);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            prepare_containerd_image(request(
                &peer,
                ImagePullPolicy::IfMissing,
                Duration::from_secs(1),
            )),
        )
        .await
        .expect("whole-operation deadline must terminate the call");
        assert_eq!(result.unwrap_err(), ContainerdImageError::Timeout);
        let events = peer.events();
        assert!(
            match phase {
                Hang::Transfer => matches!(events.last(), Some(Event::Transfer(_))),
                Hang::Read => matches!(events.last(), Some(Event::Read(_))),
                Hang::Alias => matches!(events.last(), Some(Event::Create(_))),
            },
            "deadline must expire inside the requested phase: {events:?}"
        );
        if phase != Hang::Transfer {
            assert_no_transfer(&events);
        }
        assert!(peer.state.lock().unwrap().aliases.is_empty());
    }
}

#[tokio::test]
async fn index_variant_is_retained_when_config_variant_is_absent_or_null() {
    for null_variant in [false, true] {
        let mut config = json!({
            "os": "linux", "architecture": "arm64",
            "rootfs": {"type": "layers", "diff_ids": []}, "config": {}
        });
        if null_variant {
            config["variant"] = serde_json::Value::Null;
        }
        let data = fixture_with_config(config);
        let peer = Peer::start(data.state);
        let prepared = prepare_containerd_image(request(
            &peer,
            ImagePullPolicy::LocalOnly,
            Duration::from_secs(5),
        ))
        .await
        .unwrap();
        assert_eq!(prepared.platform(), &platform());
        assert_eq!(prepared.digest(), data.manifest.digest);
        let events = peer.events();
        assert_eq!(events.len(), 5);
        assert_eq!(events[0], Event::Get(REFERENCE.into()));
        assert_eq!(events[1], Event::Read(data.index.digest));
        assert_eq!(events[2], Event::Read(data.manifest.digest.clone()));
        assert_eq!(events[3], Event::Read(data.config.digest));
        let Event::Create(alias) = &events[4] else {
            panic!("expected selected manifest alias");
        };
        assert_eq!(alias.target.as_ref(), Some(&data.manifest));
        assert_no_transfer(&events);
    }
}

#[tokio::test]
async fn unspecified_variant_accepts_verified_v8_but_not_contradictory_metadata() {
    for config_variant in ["v8", "v9"] {
        let data = fixture("arm64", Some(config_variant));
        let peer = Peer::start(data.state);
        let expected = ImagePlatform::new("linux", "arm64").unwrap();
        let request = ContainerdImageRequest::new(
            &peer.socket,
            NAMESPACE,
            REFERENCE,
            expected.clone(),
            Duration::from_secs(5),
        )
        .unwrap()
        .with_pull_policy(ImagePullPolicy::IfMissing);
        let result = prepare_containerd_image(request).await;
        if config_variant == "v8" {
            let prepared = result.unwrap();
            assert_eq!(prepared.platform(), &platform());
            assert_eq!(prepared.digest(), data.manifest.digest);
            assert!(matches!(peer.events().last(), Some(Event::Create(_))));
        } else {
            assert!(matches!(
                result,
                Err(ContainerdImageError::PlatformMismatch { expected: actual_expected, .. })
                    if *actual_expected == expected
            ));
            assert_no_alias(&peer);
        }
        let events = peer.events();
        assert_eq!(events[0], Event::Get(REFERENCE.into()));
        assert_eq!(events[1], Event::Read(data.index.digest));
        assert_eq!(events[2], Event::Read(data.manifest.digest));
        assert_eq!(events[3], Event::Read(data.config.digest));
        assert_no_transfer(&events);
    }
}

#[tokio::test]
async fn direct_immutable_alias_is_verified_without_recreating_it() {
    for (architecture, variant) in [("arm64", Some("v8")), ("amd64", None)] {
        let mut data = fixture(architecture, variant);
        let expected =
            ImagePlatform::with_variant("linux", architecture, variant.map(str::to_owned)).unwrap();
        let alias = format!("registry.example/team/image@{}", data.manifest.digest);
        data.state.image.name = alias.clone();
        data.state.image.target = Some(data.manifest.clone());
        let peer = Peer::start(data.state);
        let request = ContainerdImageRequest::new(
            &peer.socket,
            NAMESPACE,
            &alias,
            expected.clone(),
            Duration::from_secs(5),
        )
        .unwrap()
        .with_pull_policy(ImagePullPolicy::IfMissing);
        let prepared = prepare_containerd_image(request).await.unwrap();
        assert_eq!(prepared.reference(), alias);
        assert_eq!(prepared.digest(), data.manifest.digest);
        assert_eq!(prepared.platform(), &expected);
        assert_eq!(
            peer.events(),
            vec![
                Event::Get(alias),
                Event::Read(data.manifest.digest),
                Event::Read(data.config.digest),
            ]
        );
        assert_no_alias(&peer);
    }
}

#[tokio::test]
async fn direct_manifest_without_variant_cannot_satisfy_explicit_variant() {
    let mut data = fixture("arm64", None);
    data.state.image.target = Some(data.manifest.clone());
    let peer = Peer::start(data.state);
    let error = prepare_containerd_image(request(
        &peer,
        ImagePullPolicy::IfMissing,
        Duration::from_secs(5),
    ))
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        ContainerdImageError::PlatformMismatch { expected, .. } if *expected == platform()
    ));
    assert_eq!(
        peer.events(),
        vec![
            Event::Get(REFERENCE.into()),
            Event::Read(data.manifest.digest),
            Event::Read(data.config.digest),
        ]
    );
    assert_no_alias(&peer);
}

#[tokio::test]
async fn returned_direct_alias_does_not_inherit_the_source_index_variant() {
    let data = fixture("arm64", None);
    let peer = Peer::start(data.state);
    let prepared = prepare_containerd_image(request(
        &peer,
        ImagePullPolicy::LocalOnly,
        Duration::from_secs(5),
    ))
    .await
    .unwrap();
    assert_eq!(prepared.platform(), &platform());
    assert_eq!(prepared.digest(), data.manifest.digest);
    let alias = prepared.reference().to_owned();
    {
        let state = peer.state.lock().unwrap();
        assert_eq!(state.aliases.get(&alias), Some(&data.manifest));
    }
    let before = peer.events().len();
    let direct_request = ContainerdImageRequest::new(
        &peer.socket,
        NAMESPACE,
        &alias,
        platform(),
        Duration::from_secs(5),
    )
    .unwrap()
    .with_pull_policy(ImagePullPolicy::IfMissing);
    let error = prepare_containerd_image(direct_request).await.unwrap_err();
    assert_eq!(
        error,
        ContainerdImageError::PlatformMismatch {
            expected: Box::new(platform()),
            actual: Some(ImagePlatform::new("linux", "arm64").unwrap()),
        }
    );
    let events = peer.events();
    assert_eq!(
        &events[before..],
        &[
            Event::Get(alias.clone()),
            Event::Read(data.manifest.digest.clone()),
            Event::Read(data.config.digest),
        ]
    );
    assert_no_transfer(&events);
    let state = peer.state.lock().unwrap();
    assert_eq!(state.aliases.len(), 1);
    assert_eq!(state.aliases.get(&alias), Some(&data.manifest));
}

#[tokio::test]
async fn dropping_preparation_during_transfer_never_continues_to_metadata_or_alias() {
    let mut data = fixture("arm64", Some("v8"));
    data.state.missing = true;
    let mut peer = Peer::start(data.state);
    let mut control = peer.gate_transfer();
    let mut preparing = Box::pin(prepare_containerd_image(request(
        &peer,
        ImagePullPolicy::IfMissing,
        Duration::from_secs(30),
    )));
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            entered = &mut control.entered => entered.expect("transfer handler entered"),
            result = &mut preparing => panic!("preparation completed before gated transfer: {result:?}"),
        }
    }).await.expect("reach transfer gate");

    // Drop the owned public future, not merely a pinned borrow. The peer's
    // completion guard also signals if tonic cancels its handler at this point.
    drop(preparing);
    let _ = control.release.send(());
    tokio::time::timeout(Duration::from_secs(5), control.finished)
        .await
        .expect("transfer handler termination")
        .expect("transfer handler completion guard");
    tokio::time::timeout(Duration::from_secs(5), peer.finish())
        .await
        .expect("peer graceful cleanup");

    let events = peer.events();
    assert_eq!(
        events.len(),
        2,
        "no client continuation after cancellation: {events:?}"
    );
    assert_eq!(events[0], Event::Get(REFERENCE.into()));
    assert!(matches!(events[1], Event::Transfer(_)));
    assert_no_alias(&peer);
}
