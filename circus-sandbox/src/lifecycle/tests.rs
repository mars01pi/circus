use std::time::Duration;

use super::*;

async fn rejected_without_worker(request: CommandRequest, expected: CommandError) {
    let (tx, mut rx) = mpsc::channel(QUEUE);
    let sandbox = Sandbox {
        tx,
        shutdown: CancellationToken::new(),
        cleanup: cleanup::channel("unused-test-instance".to_string()).1,
        max_file_bytes: 1024,
        recovery_id: RecoveryId::generate(),
    };
    let err = tokio::time::timeout(Duration::from_secs(1), sandbox.exec(request))
        .await
        .expect("invalid command must not wait for a worker")
        .unwrap_err();
    assert!(
        !matches!(err, SandboxError::RelativePath { .. }),
        "invalid command was classified as a path failure: {err:?}"
    );
    assert!(matches!(&err, SandboxError::Command(actual) if actual == &expected));
    assert_eq!(
        std::error::Error::source(&err).and_then(|source| source.downcast_ref::<CommandError>()),
        Some(&expected)
    );
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(!sandbox.shutdown.is_cancelled());
}

fn request() -> CommandRequest {
    CommandRequest {
        argv: vec!["/bin/true".into()],
        timeout: Duration::from_secs(1),
        output_limit_bytes: 1024,
    }
}

#[tokio::test]
async fn empty_command_is_not_a_path_error() {
    let mut request = request();
    request.argv.clear();
    rejected_without_worker(request, CommandError::EmptyArgv).await;
}

#[tokio::test]
async fn zero_command_timeout_is_not_a_path_error() {
    let mut request = request();
    request.timeout = Duration::ZERO;
    rejected_without_worker(
        request,
        CommandError::InvalidTimeout {
            timeout: Duration::ZERO,
        },
    )
    .await;
}

#[tokio::test]
async fn zero_command_output_limit_is_not_a_path_error() {
    let mut request = request();
    request.output_limit_bytes = 0;
    rejected_without_worker(
        request,
        CommandError::InvalidOutputLimit {
            output_limit_bytes: 0,
        },
    )
    .await;
}

use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::infrastructure::{
    Backend, BackendProcess, Capabilities, CommonSpec, EngineError, EngineErrorKind, ExecDone,
    HostReport, InstanceId, OutputLimit as EngineOutputLimit, OutputPhase as EngineOutputPhase,
    PolicyMismatch, PrepareError, ProcessSpec, Recovered, UploadEntry, UploadSource,
    WorkspaceEntry,
};
use crate::observability::SandboxMetrics;
use crate::runtime::{IoLimits, IsolationConfig, ResourceLimits, SandboxConfig};
use crate::workspace::{SnapshotBudget, Workspace, WorkspaceError};

use super::staging::TransferFailure;

type RecordedRecovery = (String, String, Option<String>);

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecordedSource {
    Directory,
    Bytes(usize),
    File(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedUpload {
    path: String,
    source: RecordedSource,
}

#[derive(Clone)]
struct FakeBackend {
    capabilities: Capabilities,
    recoveries: Arc<Mutex<VecDeque<Recovered>>>,
    recover_calls: Arc<AtomicUsize>,
    recover_requests: Arc<Mutex<Vec<RecordedRecovery>>>,
    prepare_calls: Arc<AtomicUsize>,
    create_requests: Arc<Mutex<Vec<(String, String)>>>,
    start_calls: Arc<AtomicUsize>,
    create_error: Arc<Mutex<Option<EngineError>>>,
    exec_result: Arc<Mutex<Option<Result<ExecDone, EngineError>>>>,
    report: Arc<Mutex<HostReport>>,
    slow_reports: Arc<AtomicUsize>,
    uploads: Arc<Mutex<Vec<RecordedUpload>>>,
    upload_fail_after: Arc<AtomicUsize>,
    slow_uploads: Arc<AtomicUsize>,
    live_upload_bytes: Arc<AtomicUsize>,
    max_live_upload_bytes: Arc<AtomicUsize>,
}

impl FakeBackend {
    fn new() -> Self {
        Self {
            capabilities: Capabilities {
                pause_resume: true,
                file_transport: true,
                workspace_snapshot: true,
                spawn_process: true,
            },
            recoveries: Arc::new(Mutex::new(VecDeque::new())),
            recover_calls: Arc::new(AtomicUsize::new(0)),
            recover_requests: Arc::new(Mutex::new(Vec::new())),
            prepare_calls: Arc::new(AtomicUsize::new(0)),
            create_requests: Arc::new(Mutex::new(Vec::new())),
            start_calls: Arc::new(AtomicUsize::new(0)),
            create_error: Arc::new(Mutex::new(None)),
            exec_result: Arc::new(Mutex::new(None)),
            report: Arc::new(Mutex::new(HostReport::default())),
            slow_reports: Arc::new(AtomicUsize::new(0)),
            uploads: Arc::new(Mutex::new(Vec::new())),
            upload_fail_after: Arc::new(AtomicUsize::new(usize::MAX)),
            slow_uploads: Arc::new(AtomicUsize::new(0)),
            live_upload_bytes: Arc::new(AtomicUsize::new(0)),
            max_live_upload_bytes: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    fn uploads(&self) -> Vec<RecordedUpload> {
        self.uploads.lock().expect("uploads lock").clone()
    }

    fn max_live_upload_bytes(&self) -> usize {
        self.max_live_upload_bytes.load(Ordering::SeqCst)
    }

    fn start_calls(&self) -> usize {
        self.start_calls.load(Ordering::SeqCst)
    }

    fn fail_upload_after(&self, successes: usize) {
        self.upload_fail_after.store(successes, Ordering::SeqCst);
    }

    fn slow_next_upload(&self) {
        self.slow_uploads.store(1, Ordering::SeqCst);
    }

    fn push_recovery(&self, outcome: Recovered) {
        self.recoveries
            .lock()
            .expect("recoveries lock")
            .push_back(outcome);
    }

    fn set_create_error(&self, error: EngineError) {
        *self.create_error.lock().expect("create lock") = Some(error);
    }

    fn set_exec_result(&self, result: Result<ExecDone, EngineError>) {
        *self.exec_result.lock().expect("exec lock") = Some(result);
    }

    fn sleep_next_report(&self) {
        self.slow_reports.store(1, Ordering::SeqCst);
    }
}

// The production trait returns explicit `impl Future + Send` bounds.
#[allow(clippy::manual_async_fn)]
impl Backend for FakeBackend {
    fn kind(&self) -> &'static str {
        "fake"
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn prepare(&self) -> impl Future<Output = Result<(), PrepareError>> + Send {
        self.prepare_calls.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    }

    fn create_owned(
        &self,
        name: &str,
        _spec: &CommonSpec,
        nonce: &str,
    ) -> impl Future<Output = Result<InstanceId, EngineError>> + Send {
        self.create_requests
            .lock()
            .expect("create requests lock")
            .push((name.to_string(), nonce.to_string()));
        let failure = self.create_error.lock().expect("create lock").take();
        async move {
            match failure {
                Some(error) => Err(error),
                None => Ok(InstanceId::new("fake-instance".to_string())),
            }
        }
    }

    fn recover_owned(
        &self,
        name: &str,
        nonce: &str,
        retained_id: Option<&InstanceId>,
    ) -> impl Future<Output = Recovered> + Send {
        self.recover_calls.fetch_add(1, Ordering::SeqCst);
        self.recover_requests
            .lock()
            .expect("recover requests lock")
            .push((
                name.to_string(),
                nonce.to_string(),
                retained_id.map(|id| id.as_str().to_string()),
            ));
        let outcome = self
            .recoveries
            .lock()
            .expect("recoveries lock")
            .pop_front()
            .unwrap_or(Recovered::Removed);
        async move { outcome }
    }

    fn start(&self, _id: &InstanceId) -> impl Future<Output = Result<(), EngineError>> + Send {
        self.start_calls.fetch_add(1, Ordering::SeqCst);
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
        let uploads = Arc::clone(&self.uploads);
        let fail_after = Arc::clone(&self.upload_fail_after);
        let slow = self.slow_uploads.swap(0, Ordering::SeqCst) > 0;
        let live = Arc::clone(&self.live_upload_bytes);
        let max_live = Arc::clone(&self.max_live_upload_bytes);
        async move {
            if slow {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
            let UploadEntry { path, source } = entry;
            let recorded = match source {
                UploadSource::Directory => RecordedSource::Directory,
                UploadSource::Bytes(bytes) => RecordedSource::Bytes(bytes.len()),
                UploadSource::File { file, len } => {
                    let before = uploads.lock().expect("uploads lock").len();
                    if before >= fail_after.load(Ordering::SeqCst) {
                        return Err(engine_error("injected upload failure"));
                    }
                    let size = usize::try_from(len).unwrap_or(usize::MAX);
                    let now = live.fetch_add(size, Ordering::SeqCst) + size;
                    max_live.fetch_max(now, Ordering::SeqCst);
                    // Read the owned handle with a bounded buffer. This proves
                    // the transfer source is a real file handle rather than a
                    // pre-materialized tree.
                    let mut reader = file;
                    let mut chunk = [0u8; 64 * 1024];
                    let mut read_total = 0u64;
                    loop {
                        let read = std::io::Read::read(&mut reader, &mut chunk)
                            .map_err(|err| engine_error(&format!("fake file read: {err}")))?;
                        if read == 0 {
                            break;
                        }
                        read_total += u64::try_from(read).unwrap_or(0);
                    }
                    live.fetch_sub(size, Ordering::SeqCst);
                    if read_total != len {
                        return Err(engine_error("fake upload observed a short file"));
                    }
                    RecordedSource::File(len)
                }
            };
            uploads.lock().expect("uploads lock").push(RecordedUpload {
                path,
                source: recorded,
            });
            if uploads.lock().expect("uploads lock").len() > fail_after.load(Ordering::SeqCst) {
                return Err(engine_error("injected upload failure"));
            }
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
        async { unused_engine("download_file") }
    }

    fn remove_file(
        &self,
        _id: &InstanceId,
        _user: &str,
        _workspace: &str,
        _relative: &str,
    ) -> impl Future<Output = Result<(), EngineError>> + Send {
        async { unused_engine("remove_file") }
    }

    fn exec(
        &self,
        _id: &InstanceId,
        _user: &str,
        _workdir: &str,
        _argv: &[String],
        _output_limit: usize,
    ) -> impl Future<Output = Result<ExecDone, EngineError>> + Send {
        let result = self.exec_result.lock().expect("exec lock").take();
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
    ) -> impl Future<Output = Result<BackendProcess, EngineError>> + Send {
        async { unused_engine("spawn_process") }
    }

    fn host_report(
        &self,
        _id: &InstanceId,
    ) -> impl Future<Output = Result<HostReport, EngineError>> + Send {
        let slow = self.slow_reports.swap(0, Ordering::SeqCst) > 0;
        let report = self.report.lock().expect("report lock").clone();
        async move {
            if slow {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
            Ok(report)
        }
    }

    fn list_workspace(
        &self,
        _id: &InstanceId,
        _user: &str,
        _workspace: &str,
        _max_bytes: u64,
        _max_entries: usize,
    ) -> impl Future<Output = Result<Vec<WorkspaceEntry>, EngineError>> + Send {
        async { unused_engine("list_workspace") }
    }
}

fn unused_engine<T>(operation: &str) -> Result<T, EngineError> {
    Err(engine_error(&format!("unused fake operation: {operation}")))
}

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

fn config() -> SandboxConfig {
    let limits = ResourceLimits::new(1024 * 1024, 100, 8, 8 * 1024, 1024).unwrap();
    let isolation =
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 1000, 1000).unwrap();
    let io = IoLimits::with_deadlines(
        4096,
        Duration::from_millis(200),
        Duration::from_millis(200),
        Duration::from_millis(50),
    )
    .unwrap();
    SandboxConfig::new(limits, isolation, io).unwrap()
}

fn test_state(backend: FakeBackend) -> State<FakeBackend> {
    let config = config();
    let spec = operations::common_spec(&config);
    let workspace = Workspace::new(1024).unwrap();
    workspace.write_file("kept.txt", b"kept").unwrap();
    let (cleanup, _) = cleanup::channel("fake-name".to_string());
    State {
        backend,
        spec,
        workspace: Arc::new(Mutex::new(workspace)),
        config,
        name: "fake-name".to_string(),
        nonce: uuid::Uuid::new_v4().to_string(),
        user: "1000:1000".to_string(),
        id: Some(InstanceId::new("fake-instance".to_string())),
        phase: SandboxPhase::Created,
        shutdown: CancellationToken::new(),
        metrics: SandboxMetrics::disabled(),
        create_unsettled: false,
        creation_cleanup: false,
        process: None,
        cleanup,
    }
}

fn sandbox_config_and_spec() -> (SandboxConfig, CommonSpec) {
    let config = config();
    let spec = operations::common_spec(&config);
    (config, spec)
}

#[tokio::test]
async fn destroy_retains_identity_and_staging_until_absence_is_confirmed() {
    let backend = FakeBackend::new();
    backend.push_recovery(Recovered::StillPresent {
        detail: "engine still reports the container".to_string(),
    });
    backend.push_recovery(Recovered::Removed);
    let mut state = test_state(backend);

    let (report, error) = operations::destroy_attempt(&mut state).await;
    assert_eq!(report.ownership, Ownership::StillPresent);
    assert_eq!(report.instance_id.as_deref(), Some("fake-instance"));
    let error = error.expect("destroy must not claim success while present");
    assert!(matches!(
        std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<CleanupFailure>()),
        Some(CleanupFailure::StillPresent { .. })
    ));
    match error {
        SandboxError::CleanupRequired {
            ownership, failure, ..
        } => {
            assert_eq!(ownership, Ownership::StillPresent);
            assert!(matches!(failure, CleanupFailure::StillPresent { .. }));
        }
        other => panic!("unexpected destroy error: {other:?}"),
    }
    assert_eq!(state.phase, SandboxPhase::CleanupRequired);
    assert_eq!(
        state.id.as_ref().map(InstanceId::as_str),
        Some("fake-instance")
    );
    assert_eq!(
        state
            .workspace
            .lock()
            .expect("workspace lock")
            .read_file("kept.txt")
            .unwrap(),
        b"kept"
    );

    let (report, error) = operations::destroy_attempt(&mut state).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(report.ownership, Ownership::ConfirmedAbsent);
    assert_eq!(state.phase, SandboxPhase::Destroyed);
    assert!(state.id.is_none());
    assert!(
        state
            .workspace
            .lock()
            .expect("workspace lock")
            .read_file("kept.txt")
            .is_err()
    );
}

#[tokio::test]
async fn unconfirmed_removal_is_unknown_and_keeps_ownership() {
    let backend = FakeBackend::new();
    backend.push_recovery(Recovered::Unknown {
        detail: "inspect transport failed".to_string(),
    });
    let mut state = test_state(backend);

    let (report, error) = operations::destroy_attempt(&mut state).await;
    assert_eq!(report.ownership, Ownership::Unknown);
    match error.expect("unknown ownership is not success") {
        SandboxError::CleanupRequired {
            ownership, failure, ..
        } => {
            assert_eq!(ownership, Ownership::Unknown);
            assert!(matches!(failure, CleanupFailure::Unknown { .. }));
        }
        other => panic!("unexpected destroy error: {other:?}"),
    }
    assert!(state.id.is_some());
    assert_eq!(state.phase, SandboxPhase::CleanupRequired);
}

#[tokio::test]
async fn concurrent_destroy_calls_never_report_false_success() {
    let backend = FakeBackend::new();
    for _ in 0..2 {
        backend.push_recovery(Recovered::StillPresent {
            detail: "engine reports present".to_string(),
        });
    }
    backend.push_recovery(Recovered::Removed);
    let (config, spec) = sandbox_config_and_spec();
    let sandbox = Sandbox::create_with_backend(
        backend.clone(),
        spec,
        config,
        SandboxMetrics::disabled(),
        RecoveryId::generate(),
    )
    .await
    .expect("fake creation");

    let (first, second) = tokio::join!(sandbox.destroy(), sandbox.destroy());
    for result in [first, second] {
        match result {
            Err(SandboxError::CleanupRequired { ownership, .. }) => {
                assert_eq!(ownership, Ownership::StillPresent);
            }
            other => panic!("destroy reported a false result: {other:?}"),
        }
    }
    sandbox.destroy().await.expect("retry confirms absence");
    let status = sandbox.status().await.expect("status after removal");
    assert_eq!(status.phase, SandboxPhase::Destroyed);
    assert_eq!(status.instance_id, None);
    let (third, fourth) = tokio::join!(sandbox.destroy(), sandbox.destroy());
    assert!(third.is_ok() && fourth.is_ok());
}

#[tokio::test]
async fn aggregate_output_limit_reports_stream_and_reclaims_instance() {
    let backend = FakeBackend::new();
    backend.set_exec_result(Err(EngineError {
        output: Some(EngineOutputLimit {
            limit: 8,
            phase: EngineOutputPhase::Stderr,
        }),
        kind: EngineErrorKind::Limit,
        ..engine_error("combined budget exceeded")
    }));
    backend.push_recovery(Recovered::Removed);
    let (config, spec) = sandbox_config_and_spec();
    let sandbox = Sandbox::create_with_backend(
        backend.clone(),
        spec,
        config,
        SandboxMetrics::disabled(),
        RecoveryId::generate(),
    )
    .await
    .expect("fake creation");
    sandbox.start().await.expect("fake start");

    let error = sandbox
        .exec(CommandRequest {
            argv: vec!["/bin/true".into()],
            timeout: Duration::from_secs(1),
            output_limit_bytes: 8,
        })
        .await
        .expect_err("output limit is an error");
    match error {
        SandboxError::OutputLimit {
            limit,
            phase,
            ownership,
            ..
        } => {
            assert_eq!(limit, 8);
            assert_eq!(phase, OutputPhase::Stderr);
            assert_eq!(ownership, Ownership::ConfirmedAbsent);
        }
        other => panic!("unexpected output error: {other:?}"),
    }
    assert_eq!(backend.recover_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn request_over_configured_output_bound_is_not_started_or_destroyed() {
    let backend = FakeBackend::new();
    let (config, spec) = sandbox_config_and_spec();
    let sandbox = Sandbox::create_with_backend(
        backend.clone(),
        spec,
        config,
        SandboxMetrics::disabled(),
        RecoveryId::generate(),
    )
    .await
    .expect("fake creation");
    sandbox.start().await.expect("fake start");

    match sandbox
        .exec(CommandRequest {
            argv: vec!["/bin/true".into()],
            timeout: Duration::from_secs(1),
            output_limit_bytes: 5000,
        })
        .await
    {
        Err(SandboxError::OutputLimit {
            limit,
            phase,
            ownership,
            ..
        }) => {
            assert_eq!(limit, 4096);
            assert_eq!(phase, OutputPhase::NotStarted);
            assert_eq!(ownership, Ownership::StillPresent);
        }
        other => panic!("unexpected result: {other:?}"),
    }
    assert_eq!(backend.recover_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn status_timeout_preserves_phase_and_ownership() {
    let backend = FakeBackend::new();
    backend.sleep_next_report();
    let (config, spec) = sandbox_config_and_spec();
    let sandbox = Sandbox::create_with_backend(
        backend.clone(),
        spec,
        config,
        SandboxMetrics::disabled(),
        RecoveryId::generate(),
    )
    .await
    .expect("fake creation");
    sandbox.start().await.expect("fake start");

    let error = tokio::time::timeout(Duration::from_secs(2), sandbox.status())
        .await
        .expect("status deadline is bounded by the worker")
        .expect_err("slow status is a deadline error");
    match error {
        SandboxError::TimedOut {
            operation,
            ownership,
            ..
        } => {
            assert_eq!(operation, SandboxOperation::Status);
            assert_eq!(ownership, Ownership::StillPresent);
        }
        other => panic!("unexpected status error: {other:?}"),
    }
    assert_eq!(backend.recover_calls.load(Ordering::SeqCst), 0);
    let status = sandbox.status().await.expect("retry status");
    assert_eq!(status.phase, SandboxPhase::Running);
    assert_eq!(status.instance_id.as_deref(), Some("fake-instance"));
    // The instance was not destroyed: another transition is still legal.
    sandbox
        .pause()
        .await
        .expect("timeout must leave the phase retryable");
}

#[tokio::test]
async fn policy_mismatch_fails_creation_and_destroys_the_instance() {
    let backend = FakeBackend::new();
    backend.set_create_error(EngineError {
        create_id: Some("fake-instance".to_string()),
        policy: Some(Box::new(PolicyMismatch {
            field: "network_mode",
            expected: "none".to_string(),
            actual: "bridge".to_string(),
        })),
        ..engine_error("effective policy mismatch")
    });
    backend.push_recovery(Recovered::Removed);
    let (config, spec) = sandbox_config_and_spec();

    let error = Sandbox::create_with_backend(
        backend.clone(),
        spec,
        config,
        SandboxMetrics::disabled(),
        RecoveryId::generate(),
    )
    .await
    .expect_err("mismatched policy must not create a sandbox");
    match error {
        SandboxError::PolicyMismatch {
            field,
            expected,
            actual,
        } => {
            assert_eq!(field, "network_mode");
            assert_eq!(expected, "none");
            assert_eq!(actual, "bridge");
        }
        other => panic!("unexpected create error: {other:?}"),
    }
    assert_eq!(backend.recover_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn policy_mismatch_retains_cleanup_ownership_when_removal_is_unconfirmed() {
    let backend = FakeBackend::new();
    backend.set_create_error(EngineError {
        create_id: Some("fake-instance".to_string()),
        policy: Some(Box::new(PolicyMismatch {
            field: "tmpfs",
            expected: "size=8192".to_string(),
            actual: "size=1".to_string(),
        })),
        ..engine_error("effective policy mismatch")
    });
    backend.push_recovery(Recovered::StillPresent {
        detail: "removal not confirmed".to_string(),
    });
    backend.push_recovery(Recovered::Removed);
    let (config, spec) = sandbox_config_and_spec();

    let error = Sandbox::create_with_backend(
        backend.clone(),
        spec,
        config,
        SandboxMetrics::disabled(),
        RecoveryId::generate(),
    )
    .await
    .expect_err("mismatched policy must not create a sandbox");
    match error {
        SandboxError::CreationCleanupRequired {
            instance_id,
            source,
            cleanup,
        } => {
            assert_eq!(instance_id, "fake-instance");
            assert!(matches!(
                source.as_ref(),
                SandboxError::PolicyMismatch { field, .. } if field == "tmpfs"
            ));
            let report = cleanup.wait().await.expect("retry confirms absence");
            assert_eq!(report.ownership, Ownership::ConfirmedAbsent);
        }
        other => panic!("unexpected create error: {other:?}"),
    }
}

#[tokio::test]
async fn cancelled_cleanup_wait_does_not_stop_owned_retry() {
    let backend = FakeBackend::new();
    backend.set_create_error(EngineError {
        create_id: Some("fake-instance".to_string()),
        policy: Some(Box::new(PolicyMismatch {
            field: "cap_drop",
            expected: "ALL".to_string(),
            actual: "none".to_string(),
        })),
        ..engine_error("effective policy mismatch")
    });
    backend.push_recovery(Recovered::StillPresent {
        detail: "first removal unconfirmed".to_string(),
    });
    backend.push_recovery(Recovered::Removed);
    let (config, spec) = sandbox_config_and_spec();

    let error = Sandbox::create_with_backend(
        backend.clone(),
        spec,
        config,
        SandboxMetrics::disabled(),
        RecoveryId::generate(),
    )
    .await
    .expect_err("mismatched policy must not create a sandbox");
    let SandboxError::CreationCleanupRequired { cleanup, .. } = error else {
        panic!("expected a creation cleanup receipt");
    };
    // Cancelling a wait is only an observation change; the worker keeps going.
    assert!(
        tokio::time::timeout(Duration::from_millis(1), cleanup.wait())
            .await
            .is_err(),
        "cleanup completed before the wait could be cancelled"
    );
    let report = cleanup.wait().await.expect("retained retry completes");
    assert_eq!(report.ownership, Ownership::ConfirmedAbsent);
}

fn state_with_staging(
    backend: FakeBackend,
    workspace_bytes: u64,
    max_file_bytes: usize,
    dirs: &[&str],
    files: &[(&str, &[u8])],
) -> State<FakeBackend> {
    let limits =
        ResourceLimits::new(64 * 1024 * 1024, 100, 8, workspace_bytes, max_file_bytes).unwrap();
    let isolation =
        IsolationConfig::linux_enforced(true, true, true, true, "/workspace", 1000, 1000).unwrap();
    let io = IoLimits::with_deadlines(
        4096,
        Duration::from_millis(200),
        Duration::from_millis(200),
        Duration::from_millis(50),
    )
    .unwrap();
    let config = SandboxConfig::new(limits, isolation, io).unwrap();
    let spec = operations::common_spec(&config);
    let workspace = Workspace::new(max_file_bytes).unwrap();
    for dir in dirs {
        workspace.create_dir_all(dir).unwrap();
    }
    for (path, bytes) in files {
        workspace.write_file(path, bytes).unwrap();
    }
    let (cleanup, _) = cleanup::channel("fake-name".to_string());
    State {
        backend,
        spec,
        workspace: Arc::new(Mutex::new(workspace)),
        config,
        name: "fake-name".to_string(),
        nonce: uuid::Uuid::new_v4().to_string(),
        user: "1000:1000".to_string(),
        id: Some(InstanceId::new("fake-instance".to_string())),
        phase: SandboxPhase::Created,
        shutdown: CancellationToken::new(),
        metrics: SandboxMetrics::disabled(),
        create_unsettled: false,
        creation_cleanup: false,
        cleanup,
        process: None,
    }
}

#[tokio::test]
async fn over_limit_staging_is_rejected_before_start_and_retry_succeeds() {
    let backend = FakeBackend::new();
    let mut state = state_with_staging(
        backend.clone(),
        1024,
        1024,
        &[],
        &[
            ("first", &[b'x'; 700]),
            ("second", &[b'x'; 700]),
            ("third", &[b'x'; 700]),
        ],
    );

    let error = operations::start(&mut state)
        .await
        .expect_err("aggregate staging bound must reject start");
    assert!(matches!(
        error,
        SandboxError::Staging(StagingError::LimitExceeded {
            limit: 1024,
            actual: 1400,
        })
    ));
    assert_eq!(state.phase, SandboxPhase::Created);
    assert_eq!(
        state.id.as_ref().map(InstanceId::as_str),
        Some("fake-instance")
    );
    assert_eq!(backend.start_calls(), 0, "no instance was started");
    assert!(backend.uploads().is_empty(), "no payload was transferred");

    {
        let workspace = state.workspace.lock().expect("workspace lock");
        workspace.remove_file("first").unwrap();
        workspace.remove_file("third").unwrap();
        workspace.write_file("second", &[b'x'; 1024]).unwrap();
    }
    operations::start(&mut state).await.expect("retry fits");
    assert_eq!(state.phase, SandboxPhase::Running);
    assert_eq!(backend.start_calls(), 1);
    assert_eq!(
        backend.uploads(),
        vec![RecordedUpload {
            path: "second".to_string(),
            source: RecordedSource::File(1024),
        }]
    );
}

#[tokio::test]
async fn start_streams_one_file_at_a_time_in_plan_order() {
    let backend = FakeBackend::new();
    let payload = vec![b'z'; 10 * 1024];
    let mut state = state_with_staging(
        backend.clone(),
        64 * 1024,
        16 * 1024,
        &["nested", "nested/empty"],
        &[
            ("nested/one.bin", &payload),
            ("nested/two.bin", &payload),
            ("three.bin", &payload),
        ],
    );
    operations::start(&mut state).await.expect("tree fits");
    assert_eq!(state.phase, SandboxPhase::Running);
    let uploads = backend.uploads();
    assert_eq!(uploads.len(), 5, "two dirs and three files");
    let files = uploads
        .iter()
        .filter(|upload| matches!(&upload.source, RecordedSource::File(_)))
        .count();
    assert_eq!(files, 3);
    assert!(
        uploads
            .iter()
            .all(|upload| !matches!(&upload.source, RecordedSource::Bytes(_))),
        "staged payloads must use file handles, not in-memory trees"
    );
    assert_eq!(
        backend.max_live_upload_bytes(),
        10 * 1024,
        "only one file payload may be live at a time"
    );
}

#[tokio::test]
async fn transfer_reports_a_workspace_read_error_without_uploading() {
    let backend = FakeBackend::new();
    let state = state_with_staging(backend.clone(), 4096, 1024, &[], &[("gone", b"payload")]);
    let plan = super::staging::plan(&state)
        .await
        .expect("plan walks staging");
    state
        .workspace
        .lock()
        .expect("workspace lock")
        .remove_file("gone")
        .unwrap();
    let id = state.id.clone().expect("instance id");
    let failure = super::staging::transfer(&state, &id, &plan)
        .await
        .expect_err("removed file cannot be streamed");
    match failure {
        TransferFailure::Workspace(WorkspaceError::Io(err)) => {
            assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("unexpected transfer failure: {other:?}"),
    }
    assert!(backend.uploads().is_empty());
}

#[tokio::test]
async fn partial_transfer_failure_destroys_the_started_instance() {
    let backend = FakeBackend::new();
    backend.fail_upload_after(1);
    let mut state = state_with_staging(
        backend.clone(),
        4096,
        1024,
        &[],
        &[("first", b"one"), ("second", b"two")],
    );
    let error = operations::start(&mut state)
        .await
        .expect_err("second upload fails");
    assert!(matches!(error, SandboxError::Backend { .. }));
    assert_eq!(state.phase, SandboxPhase::Destroyed);
    assert_eq!(backend.uploads().len(), 1);
}

#[tokio::test]
async fn timed_out_start_transfer_never_reports_running_and_retains_cleanup() {
    let backend = FakeBackend::new();
    backend.slow_next_upload();
    let (config, spec) = sandbox_config_and_spec();
    let sandbox = Sandbox::create_with_backend(
        backend.clone(),
        spec,
        config,
        SandboxMetrics::disabled(),
        RecoveryId::generate(),
    )
    .await
    .expect("fake creation succeeds");
    sandbox
        .write_file("slow", b"payload")
        .await
        .expect("staging write succeeds");
    let error = sandbox
        .start()
        .await
        .expect_err("transfer exceeds deadline");
    assert!(matches!(
        error,
        SandboxError::TimedOut {
            operation: SandboxOperation::Start,
            ..
        }
    ));
    assert_eq!(
        sandbox.status().await.unwrap().phase,
        SandboxPhase::Destroyed
    );
}

#[tokio::test]
async fn creation_succeeds_without_pause_snapshot_or_process_capabilities() {
    let backend = FakeBackend::new().with_capabilities(Capabilities {
        pause_resume: false,
        file_transport: true,
        workspace_snapshot: false,
        spawn_process: false,
    });
    let sandbox = Sandbox::create_for_test(backend, config())
        .await
        .expect("a workload backend without optional capabilities still creates");
    sandbox.start().await.expect("start is supported");

    let error = sandbox.pause().await.expect_err("pause is unsupported");
    assert!(matches!(
        error,
        SandboxError::UnsupportedCapability {
            capability: "pause and resume"
        }
    ));
    let error = sandbox.resume().await.expect_err("resume is unsupported");
    assert!(matches!(
        error,
        SandboxError::UnsupportedCapability {
            capability: "pause and resume"
        }
    ));
    // The rejected transitions neither changed phase nor destroyed the sandbox.
    assert_eq!(sandbox.status().await.unwrap().phase, SandboxPhase::Running);

    let error = sandbox
        .capture_workspace_snapshot(SnapshotBudget::new(1024, 4).unwrap(), 1024)
        .await
        .expect_err("capture is unsupported");
    assert!(matches!(
        error,
        SandboxError::UnsupportedCapability {
            capability: "capture a workspace snapshot"
        }
    ));
    let snapshot = Workspace::new(1024)
        .unwrap()
        .capture_code_snapshot(SnapshotBudget::new(1024, 4).unwrap())
        .unwrap();
    let error = sandbox
        .restore_workspace_snapshot(Arc::new(snapshot))
        .await
        .expect_err("restore is unsupported");
    assert!(matches!(
        error,
        SandboxError::UnsupportedCapability {
            capability: "restore a workspace snapshot"
        }
    ));

    let error = match sandbox
        .spawn_process(ProcessRequest {
            argv: vec![
                "python3".to_string(),
                "-c".to_string(),
                "print(1)".to_string(),
            ],
            working_dir: None,
            env: std::collections::BTreeMap::new(),
            timeout: Duration::from_secs(1),
            output_limit_bytes: 1024,
        })
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("processes are unsupported"),
    };
    assert!(matches!(
        error,
        SandboxError::UnsupportedCapability {
            capability: "attach an interactive process"
        }
    ));
    assert_eq!(sandbox.status().await.unwrap().phase, SandboxPhase::Running);
    sandbox.destroy().await.expect("destroy succeeds");
}

#[tokio::test]
async fn backend_without_file_transport_is_rejected_before_allocation() {
    let backend = FakeBackend::new().with_capabilities(Capabilities {
        pause_resume: false,
        file_transport: false,
        workspace_snapshot: false,
        spawn_process: false,
    });
    let error = Sandbox::create_for_test(backend, config())
        .await
        .expect_err("creation needs guest file transport");
    assert!(matches!(
        error,
        SandboxError::UnsupportedCapability {
            capability: "transfer guest files"
        }
    ));
}

mod recovery_tests;
