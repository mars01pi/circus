//! Real Kubernetes workload suites: lifecycle, exec, files, interactive
//! processes, observables, typed unsupported operations, allocation-free
//! rejections, and ownership.
//!
//! Run serially (`--test-threads=1`) so allocation counts and shared namespace
//! observations are stable.

mod kubernetes_support;

use std::collections::BTreeMap;
use std::time::Duration;

use circus_sandbox::{
    BackendConfig, Ownership, ProcessExit, ProcessOutput, ProcessRequest, ProcessSession,
    ProcessTermination, Sandbox, SandboxError, SandboxPhase, WorkspaceError,
};
use kubernetes_support as support;

fn process_request(script: &str, timeout: Duration, output_limit: usize) -> ProcessRequest {
    ProcessRequest {
        argv: vec![
            "python3".to_string(),
            "-u".to_string(),
            "-c".to_string(),
            script.to_string(),
        ],
        working_dir: None,
        env: BTreeMap::new(),
        timeout,
        output_limit_bytes: output_limit,
    }
}

async fn until_stdout(process: &ProcessSession, expected: &[u8]) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut bytes = Vec::new();
        while !bytes.ends_with(expected) {
            match process.next_output().await {
                Some(ProcessOutput::Stdout(chunk)) => bytes.extend(chunk),
                Some(ProcessOutput::Stderr(_)) => {}
                None => panic!("output closed before the expected bytes"),
            }
        }
        bytes
    })
    .await
    .expect("expected stdout within the deadline")
}

async fn drain(process: &ProcessSession) -> (Vec<u8>, Vec<u8>) {
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    while let Some(output) = process.next_output().await {
        match output {
            ProcessOutput::Stdout(bytes) => stdout.extend(bytes),
            ProcessOutput::Stderr(bytes) => stderr.extend(bytes),
        }
    }
    (stdout, stderr)
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_linux_runc_lifecycle_exec_files_and_observables() -> support::Result {
    support::enabled();
    let sandbox = support::create().await?;
    assert_eq!(sandbox.status().await?.phase, SandboxPhase::Created);

    sandbox.write_file("hello.txt", b"hello").await?;
    sandbox.create_dir_all("nested").await?;
    sandbox.write_file("nested/data.txt", b"staged").await?;
    sandbox.start().await?;
    let id = sandbox
        .status()
        .await?
        .instance_id
        .ok_or("running sandbox has no Pod name")?;
    assert!(support::pod_exists(&id)?);

    // The Pod was accepted and is observable, but only inspected values are
    // reported. Requested values are not relabeled as enforcement.
    let status = sandbox.status().await?;
    assert_eq!(status.phase, SandboxPhase::Running);
    let observed = status
        .enforcement
        .as_ref()
        .expect("running sandbox exposes inspected Pod spec evidence");
    assert_eq!(observed.memory_bytes, Some(128 * 1024 * 1024));
    assert_eq!(observed.nano_cpus, Some(1_000_000_000));
    assert_eq!(observed.readonly_root, Some(true));
    assert_eq!(observed.cap_drop, Some(vec!["ALL".to_string()]));
    let security = observed
        .security_opt
        .as_ref()
        .expect("observed security options");
    assert!(
        security
            .iter()
            .any(|option| option == "seccomp:RuntimeDefault")
    );
    assert!(
        security
            .iter()
            .any(|option| option == "allow-privilege-escalation:false")
    );
    assert_eq!(observed.bind_count, Some(0));
    // No Kubernetes field proves these, so they stay absent.
    assert_eq!(observed.runtime, None);
    assert_eq!(observed.pids_limit, None);
    assert_eq!(observed.network_mode, None);
    assert_eq!(observed.tmpfs_workspace, None);

    // argv-only exec, guest working directory, exit status, and output.
    let output = sandbox
        .exec(support::python(&["-c", "import os; print(os.getcwd())"]))
        .await?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"/workspace\n");
    assert!(output.stderr.is_empty());

    let output = sandbox
        .exec(support::python(&["-c", "import sys; sys.exit(7)"]))
        .await?;
    assert_eq!(output.exit_code, 7);

    // Staged files survive the transfer and are readable through the guest.
    assert_eq!(sandbox.read_file("hello.txt").await?, b"hello");
    assert_eq!(sandbox.read_file("nested/data.txt").await?, b"staged");
    sandbox.remove_file("nested/data.txt").await?;
    assert!(sandbox.read_file("nested/data.txt").await.is_err());

    let too_big = vec![b'x'; 1024 * 1024 + 1];
    let error = sandbox
        .write_file("too-big", &too_big)
        .await
        .expect_err("per-file bound is enforced");
    assert!(matches!(
        error,
        SandboxError::Workspace(WorkspaceError::LimitExceeded { .. })
    ));

    sandbox.destroy().await?;
    support::assert_absent(&id)?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_interactive_process_round_trip_and_nonzero_exit() -> support::Result {
    support::enabled();
    let sandbox = support::create().await?;
    let id = sandbox
        .status()
        .await?
        .instance_id
        .ok_or("created sandbox has no Pod name")?;
    sandbox.create_dir_all("project").await?;
    sandbox.start().await?;
    assert!(support::pod_exists(&id)?);

    let mut request = process_request(
        "import os,sys\nprint('ready:' + os.getcwd(), flush=True)\nfor line in sys.stdin:\n    print('reply:' + line.strip(), flush=True)\nsys.stderr.write('marker:' + os.environ['CIRCUS_PROCESS_MARKER'] + '\\n')\nsys.stderr.flush()\nsys.exit(7)\n",
        Duration::from_secs(20),
        64 * 1024,
    );
    // Lifecycle resolves the relative working directory below the workspace and
    // forwards explicit overrides; Kubernetes applies both inside the trampoline.
    request.working_dir = Some("project".to_string());
    request.env = BTreeMap::from([(
        "CIRCUS_PROCESS_MARKER".to_string(),
        "left=right".to_string(),
    )]);
    let process = sandbox.spawn_process(request).await?;
    assert_eq!(
        until_stdout(&process, b"ready:/workspace/project\n").await,
        b"ready:/workspace/project\n"
    );

    // Only one interactive process is admitted per sandbox.
    let error = match sandbox
        .spawn_process(process_request(
            "import time; time.sleep(5)",
            Duration::from_secs(5),
            1024,
        ))
        .await
    {
        Ok(_) => panic!("a second process is rejected while one is attached"),
        Err(error) => error,
    };
    assert!(matches!(error, SandboxError::ProcessActive));

    // Files remain usable while the process runs.
    sandbox.write_file("while-running.txt", b"parallel").await?;

    for message in ["one", "two"] {
        process
            .write_stdin(format!("{message}\n").as_bytes())
            .await?;
        assert_eq!(
            until_stdout(&process, b"\n").await,
            format!("reply:{message}\n").as_bytes()
        );
    }
    process.close_stdin().await?;
    let (stdout, stderr) = drain(&process).await;
    assert!(stdout.is_empty(), "unexpected trailing stdout: {stdout:?}");
    assert_eq!(stderr, b"marker:left=right\n");
    assert_eq!(process.wait().await?, ProcessExit::Exited { exit_code: 7 });

    // A normal exit retains the Pod and its workspace.
    assert_eq!(sandbox.status().await?.phase, SandboxPhase::Running);
    assert_eq!(sandbox.read_file("while-running.txt").await?, b"parallel");

    support::destroy_and_assert(&sandbox, &id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_interactive_process_cancellation_cleans_up() -> support::Result {
    support::enabled();
    let sandbox = support::create().await?;
    sandbox.start().await?;
    let id = sandbox
        .status()
        .await?
        .instance_id
        .ok_or("running sandbox has no Pod name")?;

    let process = sandbox
        .spawn_process(process_request(
            "import time\nprint('sleeping', flush=True)\ntime.sleep(120)\n",
            Duration::from_secs(60),
            64 * 1024,
        ))
        .await?;
    until_stdout(&process, b"sleeping\n").await;
    process.cancel();
    match tokio::time::timeout(Duration::from_secs(20), process.wait())
        .await
        .expect("process outcome within the deadline")?
    {
        ProcessExit::Terminated { reason, cleanup } => {
            assert_eq!(reason, ProcessTermination::Cancelled);
            assert_eq!(cleanup.ownership, Ownership::ConfirmedAbsent);
        }
        other => panic!("expected cancellation, got {other:?}"),
    }
    assert_eq!(sandbox.status().await?.phase, SandboxPhase::Destroyed);
    support::eventually_absent(&id).await?;
    assert!(sandbox.destroy().await.is_ok());
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_exec_output_limit_destroys_and_confirms_absence() -> support::Result {
    support::enabled();
    let sandbox = support::create().await?;
    sandbox.start().await?;
    let id = sandbox
        .status()
        .await?
        .instance_id
        .ok_or("running sandbox has no Pod name")?;

    let mut request = support::python(&["-c", "print('x' * 100)"]);
    request.output_limit_bytes = 8;
    let error = sandbox
        .exec(request)
        .await
        .expect_err("stdout exceeds the shared budget");
    assert!(matches!(error, SandboxError::OutputLimit { .. }));
    assert_eq!(sandbox.status().await?.phase, SandboxPhase::Destroyed);
    support::assert_absent(&id)?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_unsupported_operations_are_typed() -> support::Result {
    support::enabled();
    let sandbox = support::create().await?;
    sandbox.start().await?;

    for error in [
        sandbox.pause().await.expect_err("pause is unsupported"),
        sandbox.resume().await.expect_err("resume is unsupported"),
    ] {
        assert!(matches!(
            error,
            SandboxError::UnsupportedCapability {
                capability: "pause and resume"
            }
        ));
    }

    // Pause/resume rejection left the sandbox running and owned. Interactive
    // processes and exec are supported and are exercised by their own suites.
    assert_eq!(sandbox.status().await?.phase, SandboxPhase::Running);
    sandbox.destroy().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_invalid_image_is_rejected_before_allocation() -> support::Result {
    support::enabled();
    let before = support::managed_pod_count()?;
    let error = Sandbox::create(
        support::backend_in(&support::namespace(), "Upper/Invalid Image"),
        support::config()?,
    )
    .await
    .expect_err("malformed image reference is rejected");
    assert!(
        support::image_reference_rejected(&error),
        "unexpected error: {error}"
    );
    assert_eq!(support::managed_pod_count()?, before);
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_unreachable_api_is_typed_and_allocates_nothing() -> support::Result {
    support::enabled();
    let (path, kubeconfig) = support::unreachable_kubeconfig("unreachable")?;
    let before = support::managed_pod_count()?;
    let backend = BackendConfig::kubernetes(&kubeconfig, support::namespace(), support::image());
    let error = Sandbox::create(backend, support::config()?)
        .await
        .expect_err("unreachable API fails preparation");
    std::fs::remove_file(path).ok();
    assert!(
        support::is_backend_error(&error),
        "unexpected error: {error}"
    );
    assert_eq!(support::managed_pod_count()?, before);
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_absent_image_fails_start_without_leaking() -> support::Result {
    support::enabled();
    let sandbox = Sandbox::create(
        support::backend_in(&support::namespace(), &support::absent_image()),
        support::config()?,
    )
    .await?;
    let id = sandbox
        .status()
        .await?
        .instance_id
        .ok_or("created sandbox has no Pod name")?;
    let error = sandbox.start().await.expect_err("the pull cannot succeed");
    assert!(
        matches!(
            error,
            SandboxError::Backend { .. } | SandboxError::TimedOut { .. }
        ),
        "unexpected start error: {error}"
    );
    assert_eq!(sandbox.status().await?.phase, SandboxPhase::Destroyed);
    support::assert_absent(&id)?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_foreign_pod_with_same_name_is_never_deleted() -> support::Result {
    support::enabled();
    let sandbox = support::create().await?;
    sandbox.start().await?;
    let id = sandbox
        .status()
        .await?
        .instance_id
        .ok_or("running sandbox has no Pod name")?;

    // Replace the owned Pod with an unrelated Pod carrying the same name and
    // no owner label. Cleanup must refuse to delete it.
    support::kubectl(&["delete", "pod", &id, "--grace-period=0", "--force"])?;
    for _ in 0..50 {
        if !support::pod_exists(&id)? {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!support::pod_exists(&id)?, "owned Pod was not deleted");
    let image = format!("--image={}", support::image());
    support::kubectl(&[
        "run",
        &id,
        &image,
        "--restart=Never",
        "--labels=app=foreign",
        "--",
        "python3",
        "-c",
        "import time; time.sleep(600)",
    ])?;
    for _ in 0..50 {
        if support::pod_exists(&id)? {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(support::pod_exists(&id)?, "foreign Pod was not created");

    let error = sandbox
        .destroy()
        .await
        .expect_err("foreign Pod must block cleanup");
    assert!(
        matches!(
            error,
            SandboxError::CleanupRequired {
                ownership: circus_sandbox::Ownership::Unknown,
                ..
            }
        ),
        "unexpected destroy error: {error}"
    );
    assert!(
        support::pod_exists(&id)?,
        "foreign Pod was deleted by cleanup"
    );

    // Removing the foreign Pod lets the retained ownership settle.
    support::kubectl(&["delete", "pod", &id, "--grace-period=0", "--force"])?;
    for _ in 0..50 {
        if !support::pod_exists(&id)? {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    support::assert_absent(&id)?;
    sandbox.destroy().await?;
    Ok(())
}
