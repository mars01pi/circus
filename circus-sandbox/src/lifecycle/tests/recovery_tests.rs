use std::sync::atomic::Ordering;

use super::*;
use crate::infrastructure::Recovered;
use crate::lifecycle::recovery::recover_orphan_with_backend;

#[test]
fn recovery_ids_are_generated_and_parsed_only_in_canonical_form() {
    for _ in 0..32 {
        let generated = RecoveryId::generate();
        assert_eq!(generated.as_str().len(), 36);
        assert!(
            generated
                .as_str()
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte) || byte == b'-')
        );
        assert_eq!(RecoveryId::parse(generated.as_str()).unwrap(), generated);
        assert_eq!(generated.as_str().parse::<RecoveryId>().unwrap(), generated);
        assert_eq!(generated.to_string(), generated.as_str());
    }

    let canonical = RecoveryId::generate();
    let rejected = [
        canonical.as_str().to_ascii_uppercase(),
        canonical.as_str().replace('-', ""),
        format!("circus-sandbox-{canonical}"),
        format!("urn:uuid:{canonical}"),
        format!("{{{canonical}}}"),
        format!(" {canonical}"),
        format!("{canonical}/../escape"),
        "../circus-sandbox".to_string(),
        String::new(),
    ];
    for value in rejected {
        let error = RecoveryId::parse(&value).expect_err("non-canonical id must be rejected");
        assert_eq!(error, RecoveryIdError);
        assert_eq!(
            error.to_string(),
            "recovery id must be a canonical lowercase hyphenated UUID"
        );
        if !value.is_empty() {
            assert!(
                !error.to_string().contains(&value),
                "error echoed the rejected value"
            );
        }
    }
    assert!("not-a-uuid".parse::<RecoveryId>().is_err());
}

#[tokio::test]
async fn create_with_recovery_id_uses_exact_name_and_nonce() {
    let backend = FakeBackend::new();
    let recovery_id = RecoveryId::generate();
    let (config, spec) = sandbox_config_and_spec();

    let sandbox = Sandbox::create_with_backend(
        backend.clone(),
        spec,
        config,
        SandboxMetrics::disabled(),
        recovery_id.clone(),
    )
    .await
    .expect("fake creation");

    assert_eq!(sandbox.recovery_id(), &recovery_id);
    assert_eq!(
        backend
            .create_requests
            .lock()
            .expect("create requests lock")
            .as_slice(),
        &[(
            format!("circus-sandbox-{recovery_id}"),
            recovery_id.to_string()
        )]
    );
    assert_eq!(backend.prepare_calls.load(Ordering::SeqCst), 1);
    assert_eq!(backend.recover_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        sandbox.status().await.expect("fake status").instance_id,
        Some("fake-instance".to_string())
    );
    sandbox.destroy().await.expect("fake cleanup");
}

#[tokio::test]
async fn default_creation_generates_a_fresh_canonical_recovery_id() {
    let first_backend = FakeBackend::new();
    let first = Sandbox::create_for_test(first_backend.clone(), config())
        .await
        .expect("first fake creation");
    let second_backend = FakeBackend::new();
    let second = Sandbox::create_for_test(second_backend, config())
        .await
        .expect("second fake creation");

    assert_ne!(first.recovery_id(), second.recovery_id());
    for sandbox in [&first, &second] {
        assert_eq!(
            RecoveryId::parse(sandbox.recovery_id().as_str()).unwrap(),
            *sandbox.recovery_id()
        );
    }
    assert_eq!(
        first_backend
            .create_requests
            .lock()
            .expect("create requests lock")
            .as_slice(),
        &[(
            format!("circus-sandbox-{}", first.recovery_id()),
            first.recovery_id().to_string()
        )]
    );
    first.destroy().await.expect("first fake cleanup");
    second.destroy().await.expect("second fake cleanup");
}

#[tokio::test]
async fn recover_orphan_maps_all_outcomes_without_prepare_or_create() {
    let recovery_id = RecoveryId::generate();
    let expected = [
        (Recovered::Removed, Ownership::ConfirmedAbsent),
        (Recovered::Absent, Ownership::ConfirmedAbsent),
        (
            Recovered::StillPresent {
                detail: "engine still reports the container".to_string(),
            },
            Ownership::StillPresent,
        ),
        (
            Recovered::Unknown {
                detail: "inspect transport failed".to_string(),
            },
            Ownership::Unknown,
        ),
    ];
    for (recovered, ownership) in expected {
        let backend = FakeBackend::new();
        backend.push_recovery(recovered);
        assert_eq!(
            recover_orphan_with_backend(&backend, &recovery_id).await,
            ownership
        );
        assert_eq!(backend.recover_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.prepare_calls.load(Ordering::SeqCst), 0);
        assert!(
            backend
                .create_requests
                .lock()
                .expect("create requests lock")
                .is_empty()
        );
        assert_eq!(
            backend
                .recover_requests
                .lock()
                .expect("recover requests lock")
                .as_slice(),
            &[(
                format!("circus-sandbox-{recovery_id}"),
                recovery_id.to_string(),
                None
            )]
        );
    }
}
