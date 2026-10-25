use std::io;
use std::path::Path;

use super::{SnapshotBudget, SnapshotError, Workspace};

#[test]
fn zero_budget_and_file_cap_are_rejected() {
    assert!(matches!(
        Workspace::new(0),
        Err(super::super::WorkspaceError::InvalidLimit { max_file_bytes: 0 })
    ));
    assert!(matches!(
        SnapshotBudget::new(0, 1),
        Err(SnapshotError::InvalidBudget {
            max_total_bytes: 0,
            ..
        })
    ));
}

#[test]
fn capture_refuses_symlinks_without_following_them() {
    let mut workspace = Workspace::new(32).unwrap();
    workspace.write_file("target", b"secret").unwrap();
    let root = workspace.temp.as_ref().unwrap().path();
    symlink_file(Path::new("target"), &root.join("alias")).unwrap();
    let error = workspace
        .capture_code_snapshot(SnapshotBudget::new(32, 8).unwrap())
        .unwrap_err();
    assert!(matches!(error, SnapshotError::Symlink { path } if path == Path::new("alias")));
    assert_eq!(workspace.read_file("target").unwrap(), b"secret");
    workspace.cleanup().unwrap();
}

#[cfg(unix)]
#[test]
fn capture_refuses_nonregular_fifo() {
    let mut workspace = Workspace::new(32).unwrap();
    let fifo = workspace.temp.as_ref().unwrap().path().join("pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("spawn mkfifo");
    assert!(status.success());
    let error = workspace
        .capture_code_snapshot(SnapshotBudget::new(32, 8).unwrap())
        .unwrap_err();
    assert!(matches!(error, SnapshotError::NotRegular { path } if path == Path::new("pipe")));
    workspace.cleanup().unwrap();
}

#[cfg(unix)]
#[test]
fn capture_rejects_nested_windows_drive_component() {
    let mut workspace = Workspace::new(32).unwrap();
    workspace.create_dir_all("dir").unwrap();
    let path = workspace
        .temp
        .as_ref()
        .unwrap()
        .path()
        .join("dir")
        .join("C:outside");
    std::fs::write(&path, b"no").unwrap();
    let error = workspace
        .capture_code_snapshot(SnapshotBudget::new(32, 8).unwrap())
        .unwrap_err();
    assert!(matches!(
        error,
        SnapshotError::Workspace(super::WorkspaceError::PathRejected { .. })
    ));
    workspace.cleanup().unwrap();
}

#[cfg(windows)]
#[test]
fn failed_restore_surfaces_cleanup_failure() {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    let workspace = Workspace::new(8).unwrap();
    let path = workspace.temp.as_ref().unwrap().path().join("held");
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
        .unwrap();
    let error = super::snapshot::abandon_failed_restore(
        workspace,
        SnapshotError::Workspace(io::Error::other("restore failed").into()),
    );
    let SnapshotError::RestoreCleanup { destination, .. } = error else {
        panic!("expected restore cleanup error");
    };
    drop(held);
    let mut destination = *destination;
    destination.cleanup().unwrap();
    assert!(matches!(
        destination.read_file("held"),
        Err(super::WorkspaceError::Closed)
    ));
}

#[cfg(unix)]
#[test]
fn open_child_dir_does_not_block_on_fifo_replacement() {
    let mut workspace = Workspace::new(32).unwrap();
    let root = workspace.temp.as_ref().unwrap().path();
    std::fs::create_dir(root.join("victim")).unwrap();
    std::fs::remove_dir(root.join("victim")).unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(root.join("victim"))
        .status()
        .expect("spawn mkfifo");
    assert!(status.success());
    let error = super::snapshot::open_child_dir(
        workspace.dir.as_ref().unwrap(),
        Path::new("victim"),
        Path::new("victim"),
    )
    .unwrap_err();
    assert!(matches!(error, SnapshotError::NotRegular { path } if path == Path::new("victim")));
    workspace.cleanup().unwrap();
}
fn symlink_file(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link)
    }
}
