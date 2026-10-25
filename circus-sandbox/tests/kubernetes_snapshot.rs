//! Real Kubernetes workspace snapshot suite: guest emptyDir capture, restore
//! into a separate Pod, a captured empty directory, budget and unsafe-node
//! rejection, and target-side restore bounds.
//!
//! The source files are written by the guest through `pods/exec`, so capture
//! reads the Pod's own workspace rather than host staging. Every test is
//! ignored unless `CIRCUS_KUBERNETES=1`; absence is asserted with the real
//! `kubectl` CLI. Run serially (`--test-threads=1`).

mod kubernetes_support;

use std::sync::Arc;

use circus_sandbox::{
    Sandbox, SandboxError, SandboxPhase, SnapshotBudget, SnapshotError, WorkspaceError,
};
use kubernetes_support as support;

const ARCHIVE_BUDGET: u64 = 1024 * 1024;
const MIB: usize = 1024 * 1024;

fn script(code: &str) -> circus_sandbox::CommandRequest {
    support::python(&["-c", code])
}

async fn id_of(sandbox: &Sandbox) -> support::Result<String> {
    sandbox
        .status()
        .await?
        .instance_id
        .ok_or_else(|| "sandbox has no Pod name".into())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_captures_guest_workspace_and_restores_into_a_new_pod() -> support::Result {
    support::enabled();
    let source = support::create().await?;
    let source_id = id_of(&source).await?;
    source.start().await?;

    // Files (and one empty directory) exist only in the Pod's emptyDir.
    let output = source
        .exec(script(
            "from pathlib import Path\n\
             Path('alpha.txt').write_bytes(b'alpha')\n\
             Path('nested').mkdir()\n\
             Path('nested/beta.txt').write_bytes(b'beta')\n\
             Path('nested/runtime.txt').write_bytes(b'runtime')\n\
             Path('empty').mkdir()\n\
             print('written')\n",
        ))
        .await?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"written\n");

    let snapshot = source
        .capture_workspace_snapshot(support::budget()?, ARCHIVE_BUDGET)
        .await?;
    assert_eq!(snapshot.entry_count(), 5);
    assert_eq!(snapshot.total_bytes(), 5 + 4 + 7);
    assert_eq!(snapshot.max_file_bytes(), MIB);

    // Written after capture, so it proves the target receives the snapshot and
    // not the source's later state.
    source.write_file("after.txt", b"after").await?;

    let target = support::create().await?;
    let target_id = id_of(&target).await?;
    target
        .restore_workspace_snapshot(Arc::clone(&snapshot))
        .await?;
    target.start().await?;

    assert_eq!(target.read_file("alpha.txt").await?, b"alpha");
    assert_eq!(target.read_file("nested/beta.txt").await?, b"beta");
    assert_eq!(target.read_file("nested/runtime.txt").await?, b"runtime");
    assert!(target.read_file("after.txt").await.is_err());
    let output = target
        .exec(script(
            "import os\nassert os.path.isdir('empty')\nprint('empty-dir')\n",
        ))
        .await?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"empty-dir\n");

    // Capture did not disturb the running source.
    assert_eq!(source.status().await?.phase, SandboxPhase::Running);
    assert_eq!(source.read_file("alpha.txt").await?, b"alpha");

    support::destroy_and_assert(&source, &source_id).await?;
    support::destroy_and_assert(&target, &target_id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_entry_byte_and_file_limits_fail_typed_and_preserve_the_pod() -> support::Result
{
    support::enabled();
    let sandbox = support::create().await?;
    let id = id_of(&sandbox).await?;
    sandbox.start().await?;
    let output = sandbox
        .exec(script(
            "from pathlib import Path\n\
             Path('a.txt').write_bytes(b'alpha')\n\
             Path('b.txt').write_bytes(b'bee')\n\
             print('small')\n",
        ))
        .await?;
    assert_eq!(output.exit_code, 0);

    // Root entries: a.txt, b.txt. A one-entry budget stops at the second.
    let error = sandbox
        .capture_workspace_snapshot(SnapshotBudget::new(1024, 1)?, ARCHIVE_BUDGET)
        .await
        .expect_err("entry budget is enforced before payload reads");
    match error {
        SandboxError::Snapshot(SnapshotError::EntryLimit { limit, actual }) => {
            assert_eq!(limit, 1);
            assert_eq!(actual, 2);
        }
        other => panic!("expected a typed entry limit, got {other:?}"),
    }

    // Eight payload bytes exceed a six-byte capture budget.
    let error = sandbox
        .capture_workspace_snapshot(SnapshotBudget::new(6, 16)?, ARCHIVE_BUDGET)
        .await
        .expect_err("byte budget is enforced from the manifest");
    match error {
        SandboxError::Snapshot(SnapshotError::ByteLimit { limit, actual }) => {
            assert_eq!(limit, 6);
            assert_eq!(actual, 8);
        }
        other => panic!("expected a typed byte limit, got {other:?}"),
    }

    // The configured per-file bound also applies to guest-written files.
    let output = sandbox
        .exec(script(
            "from pathlib import Path\nPath('big').write_bytes(b'x' * (1024 * 1024 + 1))\n",
        ))
        .await?;
    assert_eq!(output.exit_code, 0);
    let error = sandbox
        .capture_workspace_snapshot(SnapshotBudget::new(4 * MIB, 16)?, ARCHIVE_BUDGET)
        .await
        .expect_err("the configured per-file bound is enforced");
    match error {
        SandboxError::Snapshot(SnapshotError::Workspace(WorkspaceError::LimitExceeded {
            limit,
            actual,
        })) => {
            assert_eq!(limit, MIB);
            assert_eq!(actual, MIB + 1);
        }
        other => panic!("expected a typed per-file limit, got {other:?}"),
    }

    // Domain failures leave an owned, usable Pod behind.
    assert_eq!(sandbox.status().await?.phase, SandboxPhase::Running);
    assert_eq!(sandbox.read_file("a.txt").await?, b"alpha");
    let output = sandbox
        .exec(script("import os\nos.unlink('big')\nprint('removed')\n"))
        .await?;
    assert_eq!(output.exit_code, 0);
    let snapshot = sandbox
        .capture_workspace_snapshot(SnapshotBudget::new(4 * MIB, 16)?, ARCHIVE_BUDGET)
        .await?;
    assert_eq!(snapshot.entry_count(), 2);

    support::destroy_and_assert(&sandbox, &id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_unsafe_guest_nodes_fail_closed_without_destroying_the_pod() -> support::Result {
    support::enabled();
    let sandbox = support::create().await?;
    let id = id_of(&sandbox).await?;
    sandbox.start().await?;
    let output = sandbox
        .exec(script(
            "from pathlib import Path\nPath('safe.txt').write_bytes(b'safe')\n",
        ))
        .await?;
    assert_eq!(output.exit_code, 0);

    // Same unsafe-node set as the Docker and containerd suites: a symlink, a
    // FIFO, and a multiply-linked regular file.
    for node in [
        "import os; os.symlink('/etc/passwd', 'link')",
        "import os; os.mkfifo('link')",
        "from pathlib import Path; import os; Path('target.txt').write_bytes(b'x'); os.link('target.txt', 'link')",
    ] {
        let output = sandbox.exec(script(node)).await?;
        assert_eq!(output.exit_code, 0, "guest setup failed for {node}");
        let error = sandbox
            .capture_workspace_snapshot(support::budget()?, ARCHIVE_BUDGET)
            .await
            .expect_err("unsafe guest nodes are not portable workspace entries");
        match error {
            SandboxError::Snapshot(SnapshotError::InvalidArchive { reason }) => {
                assert!(
                    reason.contains("exit 81"),
                    "the helper's unsafe-node status must be retained: {reason}"
                );
            }
            other => panic!("expected an invalid-archive rejection, got {other:?}"),
        }
        // The rejection is a snapshot-domain failure: the Pod stays running
        // and readable, and the next node can be tested.
        assert_eq!(sandbox.status().await?.phase, SandboxPhase::Running);
        assert_eq!(sandbox.read_file("safe.txt").await?, b"safe");
        let output = sandbox
            .exec(script(
                "import os\n\
                 os.unlink('link')\n\
                 if os.path.exists('target.txt'):\n    os.unlink('target.txt')\n\
                 print('removed')\n",
            ))
            .await?;
        assert_eq!(output.exit_code, 0);
    }

    let snapshot = sandbox
        .capture_workspace_snapshot(support::budget()?, ARCHIVE_BUDGET)
        .await?;
    assert_eq!(snapshot.entry_count(), 1);

    let target = support::create().await?;
    let target_id = id_of(&target).await?;
    target.restore_workspace_snapshot(snapshot).await?;
    target.start().await?;
    assert_eq!(target.read_file("safe.txt").await?, b"safe");
    assert!(target.read_file("link").await.is_err());

    support::destroy_and_assert(&sandbox, &id).await?;
    support::destroy_and_assert(&target, &target_id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real Kubernetes cluster"]
async fn kubernetes_restore_uses_target_bounds_and_preserves_existing_staging() -> support::Result {
    support::enabled();
    let source = support::create().await?;
    let source_id = id_of(&source).await?;
    source.start().await?;
    let output = source
        .exec(script(
            "from pathlib import Path\nPath('five.txt').write_bytes(b'fives')\n",
        ))
        .await?;
    assert_eq!(output.exit_code, 0);
    let snapshot = source
        .capture_workspace_snapshot(support::budget()?, ARCHIVE_BUDGET)
        .await?;

    // The target's own four-byte per-file cap rejects the five-byte payload.
    let narrow = Sandbox::create(
        support::backend(),
        support::config_with(4, 8 * 1024 * 1024)?,
    )
    .await?;
    let narrow_id = id_of(&narrow).await?;
    narrow.write_file("old.txt", b"old").await?;
    let error = narrow
        .restore_workspace_snapshot(Arc::clone(&snapshot))
        .await
        .expect_err("the target's per-file cap applies");
    match error {
        SandboxError::Snapshot(SnapshotError::Workspace(WorkspaceError::LimitExceeded {
            limit,
            actual,
        })) => {
            assert_eq!(limit, 4);
            assert_eq!(actual, 5);
        }
        other => panic!("expected a typed target limit, got {other:?}"),
    }
    // Validation failed before replacing staging, and the sandbox stays Created.
    assert_eq!(narrow.status().await?.phase, SandboxPhase::Created);
    narrow.start().await?;
    assert_eq!(narrow.read_file("old.txt").await?, b"old");
    assert!(narrow.read_file("five.txt").await.is_err());

    let target = support::create().await?;
    let target_id = id_of(&target).await?;
    target.restore_workspace_snapshot(snapshot).await?;
    target.start().await?;
    assert_eq!(target.read_file("five.txt").await?, b"fives");

    support::destroy_and_assert(&source, &source_id).await?;
    support::destroy_and_assert(&narrow, &narrow_id).await?;
    support::destroy_and_assert(&target, &target_id).await?;
    Ok(())
}
