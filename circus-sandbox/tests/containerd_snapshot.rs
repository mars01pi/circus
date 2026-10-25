//! Real containerd workspace snapshot capture from a running guest and restore
//! into a separate container.

mod containerd_support;

use circus_sandbox::SandboxPhase;
use containerd_support as support;

#[tokio::test]
#[ignore = "requires an explicit real containerd daemon"]
async fn containerd_captures_guest_tmpfs_and_restores_into_a_new_container() -> support::Result {
    support::enabled();
    let source = support::create().await?;
    let source_id = source.status().await?.instance_id.ok_or("missing id")?;
    source.start().await?;

    source.write_file("alpha.txt", b"alpha").await?;
    source.create_dir_all("nested").await?;
    source.write_file("nested/beta.txt", b"beta").await?;
    // Leave a process-visible file too: capture is of the guest tmpfs, not
    // host staging.
    let output = source
        .exec(support::python(&[
            "-c",
            "from pathlib import Path; Path('nested/runtime.txt').write_bytes(b'runtime'); print('ok')",
        ]))
        .await?;
    assert_eq!(output.exit_code, 0);

    let snapshot = source
        .capture_workspace_snapshot(support::budget()?, 1024 * 1024)
        .await?;

    let target = support::create().await?;
    let target_id = target.status().await?.instance_id.ok_or("missing id")?;
    target.restore_workspace_snapshot(snapshot).await?;
    target.start().await?;

    assert_eq!(target.read_file("alpha.txt").await?, b"alpha");
    assert_eq!(target.read_file("nested/beta.txt").await?, b"beta");
    assert_eq!(target.read_file("nested/runtime.txt").await?, b"runtime");

    // The source is still usable and keeps its own workspace.
    let status = source.status().await?;
    assert_eq!(status.phase, SandboxPhase::Running);
    assert_eq!(source.read_file("nested/runtime.txt").await?, b"runtime");

    support::destroy_and_assert(&source, &source_id).await?;
    support::destroy_and_assert(&target, &target_id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit real containerd daemon"]
async fn containerd_running_writes_are_visible_to_capture_and_survive_resume() -> support::Result {
    support::enabled();
    let sandbox = support::create().await?;
    let id = sandbox.status().await?.instance_id.ok_or("missing id")?;
    sandbox.start().await?;
    sandbox.write_file("first.txt", b"first").await?;
    let snapshot = sandbox
        .capture_workspace_snapshot(support::budget()?, 1024 * 1024)
        .await?;
    // Capture pauses and resumes a previously running guest.
    assert_eq!(sandbox.status().await?.phase, SandboxPhase::Running);
    sandbox.write_file("second.txt", b"second").await?;
    assert_eq!(sandbox.read_file("first.txt").await?, b"first");
    assert_eq!(sandbox.read_file("second.txt").await?, b"second");

    let restored = support::create().await?;
    let restored_id = restored.status().await?.instance_id.ok_or("missing id")?;
    restored.restore_workspace_snapshot(snapshot).await?;
    restored.start().await?;
    assert_eq!(restored.read_file("first.txt").await?, b"first");
    assert!(restored.read_file("second.txt").await.is_err());

    support::destroy_and_assert(&sandbox, &id).await?;
    support::destroy_and_assert(&restored, &restored_id).await?;
    Ok(())
}
