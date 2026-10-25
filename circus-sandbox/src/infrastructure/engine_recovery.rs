//! Delete/recovery ownership protocol and its injectable seam.
//!
//! The production implementation performs Docker inspect/stop/remove calls. A
//! private test double implements [`RemovalControl`] so transport failures and
//! stale observations are exercised without mocking the public API or leaking a
//! fake backend into production.

use std::future::Future;

use super::EngineError;

/// Result of identifying the named container and its owner label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OwnerCheck {
    /// The name is owned by this sandbox; carries the instance id.
    Owned(String),
    /// The name is absent.
    Absent,
    /// The name exists but does not carry this sandbox's owner label. It must
    /// never be removed.
    Foreign { name: String },
    /// Inspection failed for a transport or engine reason.
    Unknown { message: String },
}

/// Observed delete outcome. Only `Removed`/`Absent` confirm the instance gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Recovered {
    /// Forced removal completed and absence was confirmed afterwards.
    Removed,
    /// The name was already absent.
    Absent,
    /// Inspection still confirms the instance. Ownership is retained.
    StillPresent { detail: String },
    /// Neither absence nor presence could be confirmed. Ownership is retained.
    Unknown { detail: String },
}

/// Minimal container-control seam used by the recovery protocol.
pub(crate) trait RemovalControl {
    fn inspect_owner(
        &self,
        name: &str,
        owner_label: &str,
    ) -> impl Future<Output = OwnerCheck> + Send;

    /// Best-effort stop. A failure, including a transport failure, never
    /// prevents the forced removal that follows.
    fn stop(&self, id: &str) -> impl Future<Output = Result<(), EngineError>> + Send;

    /// Forced removal with dependent volumes.
    fn remove(&self, id: &str) -> impl Future<Output = Result<(), EngineError>> + Send;
}

/// Runs the ownership-preserving delete protocol.
///
/// Steps:
/// 1. Confirm the name still carries this sandbox's owner label.
/// 2. Stop the retained instance, ignoring stop failures.
/// 3. Force-remove it regardless of the stop result.
/// 4. Inspect again and report confirmed absence, still-present, or unknown.
///
/// If the initial inspection fails transiently, a retained id is still used for
/// a forced removal attempt; an unconfirmed name is never removed directly.
pub(crate) async fn recover_with<C: RemovalControl>(
    control: &C,
    name: &str,
    owner_label: &str,
    retained_id: Option<&str>,
) -> Recovered {
    let id = match control.inspect_owner(name, owner_label).await {
        OwnerCheck::Owned(id) => id,
        OwnerCheck::Absent => return Recovered::Absent,
        OwnerCheck::Foreign { name } => {
            return Recovered::Unknown {
                detail: format!(
                    "container {name} exists without the sandbox owner label; refusing to remove it"
                ),
            };
        }
        OwnerCheck::Unknown { message } => match retained_id {
            Some(id) => id.to_string(),
            None => {
                return Recovered::Unknown {
                    detail: format!(
                        "owner confirmation failed and no instance id was retained: {message}"
                    ),
                };
            }
        },
    };

    // A stop transport failure must not skip forced removal; record it as
    // diagnostic detail only.
    let stop_error = control.stop(&id).await.err();
    let remove_error = control.remove(&id).await.err();
    let mut notes = Vec::new();
    if let Some(error) = &stop_error {
        notes.push(format!("stop failed: {}", error.message));
    }
    if let Some(error) = &remove_error {
        notes.push(format!("forced removal failed: {}", error.message));
    }

    match control.inspect_owner(name, owner_label).await {
        OwnerCheck::Absent => Recovered::Removed,
        OwnerCheck::Owned(_) => Recovered::StillPresent {
            detail: if notes.is_empty() {
                "engine still reports the owned container after forced removal".to_string()
            } else {
                notes.join("; ")
            },
        },
        OwnerCheck::Foreign { name } => {
            if remove_error.is_none() {
                // The retained instance was removed; the name now belongs to
                // some unrelated container, which must be left alone.
                Recovered::Removed
            } else {
                Recovered::Unknown {
                    detail: format!(
                        "container {name} no longer carries the owner label and forced removal failed"
                    ),
                }
            }
        }
        OwnerCheck::Unknown { message } => {
            notes.push(format!("post-removal inspection failed: {message}"));
            Recovered::Unknown {
                detail: notes.join("; "),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::infrastructure::EngineErrorKind;

    fn engine_error(message: &str) -> EngineError {
        EngineError {
            message: message.to_string(),
            not_found: false,
            kind: EngineErrorKind::Other,
            create_pending: false,
            create_id: None,
            output: None,
            policy: None,
        }
    }

    struct FakeControl {
        checks: Mutex<VecDeque<OwnerCheck>>,
        stops: Mutex<VecDeque<Result<(), EngineError>>>,
        removes: Mutex<VecDeque<Result<(), EngineError>>>,
        stop_calls: AtomicUsize,
        remove_calls: AtomicUsize,
    }

    impl FakeControl {
        fn new(
            checks: Vec<OwnerCheck>,
            stops: Vec<Result<(), EngineError>>,
            removes: Vec<Result<(), EngineError>>,
        ) -> Self {
            Self {
                checks: Mutex::new(checks.into()),
                stops: Mutex::new(stops.into()),
                removes: Mutex::new(removes.into()),
                stop_calls: AtomicUsize::new(0),
                remove_calls: AtomicUsize::new(0),
            }
        }

        fn simple(checks: Vec<OwnerCheck>, remove: Result<(), EngineError>) -> Self {
            Self::new(checks, Vec::new(), vec![remove])
        }
    }

    impl RemovalControl for FakeControl {
        fn inspect_owner(
            &self,
            _name: &str,
            _owner_label: &str,
        ) -> impl Future<Output = OwnerCheck> + Send {
            let next = self
                .checks
                .lock()
                .expect("checks lock")
                .pop_front()
                .unwrap_or(OwnerCheck::Unknown {
                    message: "no scripted observation".to_string(),
                });
            async move { next }
        }

        fn stop(&self, _id: &str) -> impl Future<Output = Result<(), EngineError>> + Send {
            self.stop_calls.fetch_add(1, Ordering::SeqCst);
            let next = self
                .stops
                .lock()
                .expect("stops lock")
                .pop_front()
                .unwrap_or(Ok(()));
            async move { next }
        }

        fn remove(&self, _id: &str) -> impl Future<Output = Result<(), EngineError>> + Send {
            self.remove_calls.fetch_add(1, Ordering::SeqCst);
            let next = self
                .removes
                .lock()
                .expect("removes lock")
                .pop_front()
                .unwrap_or(Ok(()));
            async move { next }
        }
    }

    #[tokio::test]
    async fn stop_transport_failure_still_attempts_forced_removal() {
        let control = FakeControl::new(
            vec![OwnerCheck::Owned("id".into()), OwnerCheck::Absent],
            vec![Err(engine_error("stop transport closed"))],
            vec![Ok(())],
        );
        assert_eq!(
            recover_with(&control, "name", "owner", None).await,
            Recovered::Removed
        );
        assert_eq!(control.stop_calls.load(Ordering::SeqCst), 1);
        assert_eq!(control.remove_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn present_after_removal_is_still_present_not_success() {
        let control = FakeControl::simple(
            vec![
                OwnerCheck::Owned("id".into()),
                OwnerCheck::Owned("id".into()),
            ],
            Ok(()),
        );
        let outcome = recover_with(&control, "name", "owner", None).await;
        assert!(
            matches!(outcome, Recovered::StillPresent { .. }),
            "{outcome:?}"
        );
        assert_eq!(control.remove_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn removal_failure_followed_by_presence_is_still_present() {
        let control = FakeControl::simple(
            vec![
                OwnerCheck::Owned("id".into()),
                OwnerCheck::Owned("id".into()),
            ],
            Err(engine_error("engine busy")),
        );
        match recover_with(&control, "name", "owner", None).await {
            Recovered::StillPresent { detail } => {
                assert!(detail.contains("engine busy"), "{detail}");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn transient_owner_check_uses_retained_id_and_attempts_removal() {
        let control = FakeControl::new(
            vec![
                OwnerCheck::Unknown {
                    message: "dial unix socket: refused".into(),
                },
                OwnerCheck::Absent,
            ],
            vec![Ok(())],
            vec![Ok(())],
        );
        assert_eq!(
            recover_with(&control, "name", "owner", Some("retained")).await,
            Recovered::Removed
        );
        assert_eq!(control.remove_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn transient_owner_check_without_id_is_unknown() {
        let control = FakeControl::new(
            vec![OwnerCheck::Unknown {
                message: "dial unix socket: refused".into(),
            }],
            Vec::new(),
            Vec::new(),
        );
        assert!(matches!(
            recover_with(&control, "name", "owner", None).await,
            Recovered::Unknown { .. }
        ));
        assert_eq!(control.remove_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn absent_name_is_confirmed_absent() {
        let control = FakeControl::simple(vec![OwnerCheck::Absent], Ok(()));
        assert_eq!(
            recover_with(&control, "name", "owner", None).await,
            Recovered::Absent
        );
        assert_eq!(control.remove_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn foreign_name_is_never_removed() {
        let control = FakeControl::simple(
            vec![OwnerCheck::Foreign {
                name: "name".into(),
            }],
            Ok(()),
        );
        assert!(matches!(
            recover_with(&control, "name", "owner", None).await,
            Recovered::Unknown { .. }
        ));
        assert_eq!(control.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(control.remove_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unconfirmed_post_removal_inspection_is_unknown() {
        let control = FakeControl::simple(
            vec![
                OwnerCheck::Owned("id".into()),
                OwnerCheck::Unknown {
                    message: "inspect timeout".into(),
                },
            ],
            Ok(()),
        );
        match recover_with(&control, "name", "owner", None).await {
            Recovered::Unknown { detail } => {
                assert!(detail.contains("inspect timeout"), "{detail}");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn reused_name_after_successful_removal_is_removed() {
        let control = FakeControl::simple(
            vec![
                OwnerCheck::Owned("id".into()),
                OwnerCheck::Foreign {
                    name: "name".into(),
                },
            ],
            Ok(()),
        );
        assert_eq!(
            recover_with(&control, "name", "owner", None).await,
            Recovered::Removed
        );
    }

    #[tokio::test]
    async fn reused_name_after_failed_removal_is_unknown() {
        let control = FakeControl::simple(
            vec![
                OwnerCheck::Owned("id".into()),
                OwnerCheck::Foreign {
                    name: "name".into(),
                },
            ],
            Err(engine_error("remove failed")),
        );
        assert!(matches!(
            recover_with(&control, "name", "owner", None).await,
            Recovered::Unknown { .. }
        ));
    }
}
