use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use super::{PathRejectReason, Workspace, WorkspaceError};

#[test]
fn oversize_write_does_not_truncate_or_create() {
    let workspace = Workspace::new(4).unwrap();
    workspace.write_file("keep.txt", b"keep").unwrap();
    let limited = workspace.write_file("keep.txt", b"abcde").unwrap_err();
    assert!(matches!(
        limited,
        WorkspaceError::LimitExceeded {
            limit: 4,
            actual: 5
        }
    ));
    assert_eq!(workspace.read_file("keep.txt").unwrap(), b"keep");
    let missing = workspace.write_file("new.txt", b"abcde").unwrap_err();
    assert!(matches!(
        missing,
        WorkspaceError::LimitExceeded { actual: 5, .. }
    ));
    assert!(
        !workspace
            .temp
            .as_ref()
            .unwrap()
            .path()
            .join("new.txt")
            .exists()
    );
}

#[test]
fn dot_components_are_relative_and_round_trip() {
    let workspace = Workspace::new(32).unwrap();
    workspace.create_dir_all("foo/./bar").unwrap();
    workspace.write_file("foo/./bar/note.txt", b"dot").unwrap();
    assert_eq!(workspace.read_file("foo/bar/note.txt").unwrap(), b"dot");
}

#[test]
fn missing_parent_is_not_found_and_not_created() {
    let workspace = Workspace::new(16).unwrap();
    let err = workspace.write_file("missing/file.txt", b"x").unwrap_err();
    match err {
        WorkspaceError::Io(err) => assert_eq!(err.kind(), io::ErrorKind::NotFound),
        other => panic!("{other:?}"),
    }
    assert!(
        !workspace
            .temp
            .as_ref()
            .unwrap()
            .path()
            .join("missing")
            .exists()
    );
}

#[test]
fn directory_is_not_read_or_truncated() {
    let workspace = Workspace::new(32).unwrap();
    workspace.create_dir_all("dir").unwrap();
    workspace.write_file("dir/file.txt", b"inside").unwrap();
    assert!(matches!(
        workspace.read_file("dir").unwrap_err(),
        WorkspaceError::NotRegularFile { .. }
    ));
    assert!(matches!(
        workspace.write_file("dir", b"nope").unwrap_err(),
        WorkspaceError::NotRegularFile { .. }
    ));
    assert_eq!(workspace.read_file("dir/file.txt").unwrap(), b"inside");
    assert!(workspace.remove_file("dir").is_err());
    assert!(workspace.temp.as_ref().unwrap().path().join("dir").is_dir());
}

#[test]
fn exact_limit_and_empty_payload_succeed() {
    let workspace = Workspace::new(4).unwrap();
    workspace.write_file("empty.txt", b"").unwrap();
    assert_eq!(workspace.read_file("empty.txt").unwrap(), b"");
    workspace.write_file("max.txt", b"abcd").unwrap();
    assert_eq!(workspace.read_file("max.txt").unwrap(), b"abcd");
    workspace.write_file("max.txt", b"wxyz").unwrap();
    assert_eq!(workspace.read_file("max.txt").unwrap(), b"wxyz");
}

#[test]
fn read_stops_at_probe_without_using_planted_metadata_size() {
    let workspace = Workspace::new(4).unwrap();
    let planted = workspace.temp.as_ref().unwrap().path().join("big.bin");
    std::fs::write(&planted, vec![7u8; 64]).unwrap();
    match workspace.read_file("big.bin").unwrap_err() {
        WorkspaceError::LimitExceeded {
            limit: 4,
            actual: 5,
        } => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn missing_file_is_not_found() {
    let workspace = Workspace::new(8).unwrap();
    match workspace.read_file("absent.txt").unwrap_err() {
        WorkspaceError::Io(err) => assert_eq!(err.kind(), io::ErrorKind::NotFound),
        other => panic!("{other:?}"),
    }
}

#[test]
fn workspaces_do_not_share_files_and_debug_hides_root() {
    let first = Workspace::new(16).unwrap();
    let second = Workspace::new(16).unwrap();
    first.write_file("only-first.txt", b"a").unwrap();
    match second.read_file("only-first.txt").unwrap_err() {
        WorkspaceError::Io(err) => assert_eq!(err.kind(), io::ErrorKind::NotFound),
        other => panic!("{other:?}"),
    }
    let root = first
        .temp
        .as_ref()
        .unwrap()
        .path()
        .to_string_lossy()
        .into_owned();

    assert!(!format!("{first:?}").contains(&root));
}

#[test]
fn cleanup_is_idempotent_and_closed_wins_over_path_and_limit() {
    let mut workspace = Workspace::new(4).unwrap();
    workspace.write_file("note.txt", b"ab").unwrap();
    workspace.cleanup().unwrap();
    workspace.cleanup().unwrap();
    assert!(matches!(
        workspace.write_file(r"C:\too-long", b"abcde").unwrap_err(),
        WorkspaceError::Closed
    ));
    assert!(matches!(
        workspace.read_file("").unwrap_err(),
        WorkspaceError::Closed
    ));
    assert!(matches!(
        workspace.create_dir_all("..").unwrap_err(),
        WorkspaceError::Closed
    ));
    assert!(matches!(
        workspace.remove_file("/abs").unwrap_err(),
        WorkspaceError::Closed
    ));
}

#[test]
fn drop_removes_temporary_directory_and_files() {
    let workspace = Workspace::new(16).unwrap();
    let root = workspace.temp.as_ref().unwrap().path().to_path_buf();
    workspace.write_file("ephemeral.txt", b"gone").unwrap();
    let file = root.join("ephemeral.txt");
    assert_eq!(std::fs::read(&file).unwrap(), b"gone");
    drop(workspace);
    assert!(
        !file.exists(),
        "drop left workspace file {}",
        file.display()
    );
    assert!(
        !root.exists(),
        "drop left temporary directory {}",
        root.display()
    );
}

#[test]
fn cleanup_treats_already_deleted_root_as_success() {
    let mut workspace = Workspace::new(8).unwrap();
    let root = workspace.temp.as_ref().unwrap().path().to_path_buf();
    workspace.dir.take();
    std::fs::remove_dir_all(&root).unwrap();
    workspace.cleanup().unwrap();
    assert!(matches!(
        workspace.read_file("note.txt").unwrap_err(),
        WorkspaceError::Closed
    ));
    workspace.cleanup().unwrap();
}

/// The workspace root must not be traversable by group or other. This does not
/// isolate same-account or privileged processes; those can still open the
/// directory. The read, write, and cleanup checks only prove this process can
/// still use and remove it.
#[cfg(unix)]
#[test]
fn unix_workspace_root_has_no_group_or_other_and_round_trips() {
    use std::os::unix::fs::PermissionsExt;

    let mut workspace = Workspace::new(32).unwrap();
    let root = workspace.temp.as_ref().unwrap().path().to_path_buf();
    let mode = std::fs::metadata(&root).unwrap().permissions().mode();
    assert_eq!(
        mode & 0o077,
        0,
        "workspace root mode {mode:#o} grants group or other permissions"
    );
    workspace.write_file("note.txt", b"private").unwrap();
    assert_eq!(workspace.read_file("note.txt").unwrap(), b"private");
    workspace.cleanup().unwrap();
    assert!(matches!(
        workspace.read_file("note.txt").unwrap_err(),
        WorkspaceError::Closed
    ));
    assert!(!root.exists());
}

#[cfg(windows)]
#[test]
fn failed_cleanup_keeps_ownership_until_retry() {
    use std::os::windows::fs::OpenOptionsExt;

    // Win32 FILE_SHARE_READ | FILE_SHARE_WRITE. File::create also sets
    // FILE_SHARE_DELETE (0x4), which lets RemoveDirectory proceed.
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;

    let mut workspace = Workspace::new(16).unwrap();
    let held = workspace.temp.as_ref().unwrap().path().join("held.txt");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&held)
        .unwrap();
    let err = workspace.cleanup().unwrap_err();
    assert!(!matches!(err, WorkspaceError::Closed));
    assert!(workspace.temp.is_some());
    assert_eq!(workspace.read_file("held.txt").unwrap(), b"");
    drop(file);
    workspace.cleanup().unwrap();
    assert!(matches!(
        workspace.write_file("held.txt", b"x").unwrap_err(),
        WorkspaceError::Closed
    ));
}

#[test]
fn symlink_escape_cannot_read_or_replace_outside_file() {
    let workspace = Workspace::new(32).unwrap();
    let mut outside = tempfile::NamedTempFile::new().unwrap();
    outside.write_all(b"secret").unwrap();
    outside.flush().unwrap();
    let link = workspace.temp.as_ref().unwrap().path().join("escape");
    try_symlink(outside.path(), &link);
    assert!(workspace.read_file("escape").is_err());
    assert!(workspace.write_file("escape", b"pwned").is_err());
    assert_eq!(std::fs::read(outside.path()).unwrap(), b"secret");

    let root = workspace.temp.as_ref().unwrap().path();
    let file_name = outside.path().file_name().unwrap();
    let relative = Path::new("..").join(file_name);
    let relative_link = root.join("relative-escape");
    try_symlink(&relative, &relative_link);
    assert!(workspace.read_file("relative-escape").is_err());
    assert_eq!(std::fs::read(outside.path()).unwrap(), b"secret");
}

#[test]
fn in_cap_relative_symlink_reads_target_and_cleanup_keeps_outside_file() {
    let mut workspace = Workspace::new(32).unwrap();
    workspace.write_file("target.txt", b"inside").unwrap();
    let root = workspace.temp.as_ref().unwrap().path();
    let link = root.join("alias.txt");
    try_symlink(Path::new("target.txt"), &link);
    assert_eq!(workspace.read_file("alias.txt").unwrap(), b"inside");

    let mut outside = tempfile::NamedTempFile::new().unwrap();
    outside.write_all(b"secret").unwrap();
    outside.flush().unwrap();
    let escape = root.join("escape");
    try_symlink(outside.path(), &escape);
    assert!(workspace.read_file("escape").is_err());
    workspace.cleanup().unwrap();
    assert_eq!(std::fs::read(outside.path()).unwrap(), b"secret");
}

/// `remove_file` unlinks the final component without following it, so workspace
/// symlinks and Unix FIFOs are removed while their targets (and directories) stay.
#[test]
fn remove_file_unlinks_symlink_and_fifo_but_not_directory() {
    let workspace = Workspace::new(32).unwrap();
    workspace.write_file("target.txt", b"keep").unwrap();
    let root = workspace.temp.as_ref().unwrap().path();
    try_symlink(Path::new("target.txt"), &root.join("alias.txt"));
    workspace.remove_file("alias.txt").unwrap();
    assert!(
        !root.join("alias.txt").exists(),
        "symlink final component must be unlinked"
    );
    assert_eq!(workspace.read_file("target.txt").unwrap(), b"keep");

    #[cfg(unix)]
    {
        let fifo = root.join("pipe");
        let mkfifo = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap_or_else(|err| panic!("spawn mkfifo: {err}"));
        assert!(mkfifo.success(), "mkfifo exited {mkfifo}");
        workspace.remove_file("pipe").unwrap();
        assert!(!fifo.exists(), "FIFO must be unlinked without opening");
    }

    workspace.create_dir_all("dir").unwrap();
    assert!(
        matches!(workspace.remove_file("dir"), Err(WorkspaceError::Io(_))),
        "remove_file must not delete a directory"
    );
    workspace.write_file("dir/nested.txt", b"x").unwrap();
    assert_eq!(workspace.read_file("dir/nested.txt").unwrap(), b"x");
}

/// Unix open of a FIFO blocks until the other end is open. A symlink hides
/// that from `symlink_metadata`, and a blocking `open` never reaches the
/// handle check. The probe runs in a child so a bad open cannot hang this
/// process past [`FIFO_OPEN_DEADLINE`].
#[cfg(unix)]
#[test]
fn symlink_to_fifo_read_and_write_are_not_regular_file() {
    const CHILD: &str = "CIRCUS_SANDBOX_FIFO_CHILD";
    if std::env::var_os(CHILD).is_some() {
        probe_symlink_to_fifo();
        return;
    }

    let test_name = std::thread::current()
        .name()
        .expect("libtest names the test thread")
        .to_owned();
    let mut child =
        std::process::Command::new(std::env::current_exe().expect("current test executable"))
            .args(["--exact", &test_name])
            .env(CHILD, "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn FIFO probe");

    match wait_for_child(&mut child, FIFO_OPEN_DEADLINE) {
        Ok(None) => panic!("symlink-to-FIFO read/write still blocked after {FIFO_OPEN_DEADLINE:?}"),
        Ok(Some(_)) => {
            let output = child.wait_with_output().expect("collect probe output");
            assert!(
                output.status.success(),
                "probe failed: status {:?}\nstdout {}\nstderr {}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(err) => panic!("wait for FIFO probe: {err}"),
    }
}

#[cfg(unix)]
const FIFO_OPEN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(8);

#[cfg(unix)]
fn wait_for_child(
    child: &mut std::process::Child,
    deadline: std::time::Duration,
) -> io::Result<Option<std::process::ExitStatus>> {
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if start.elapsed() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn probe_symlink_to_fifo() {
    let workspace = Workspace::new(32).expect("workspace");
    let root = workspace.temp.as_ref().unwrap().path();
    let fifo = root.join("pipe");
    let mkfifo = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap_or_else(|err| panic!("spawn mkfifo: {err}"));
    assert!(mkfifo.success(), "mkfifo exited {mkfifo}");
    std::os::unix::fs::symlink("pipe", root.join("alias")).expect("symlink to FIFO");

    match workspace.read_file("alias") {
        Err(WorkspaceError::NotRegularFile { path }) => assert_eq!(path, Path::new("alias")),
        other => panic!("read symlink to FIFO: {other:?}"),
    }
    match workspace.write_file("alias", b"nope") {
        Err(WorkspaceError::NotRegularFile { path }) => assert_eq!(path, Path::new("alias")),
        other => panic!("write symlink to FIFO: {other:?}"),
    }
}

/// Two writers replace one file through the same [`Workspace`] while a reader
/// samples it. Each successful read must be the initial file or one complete
/// writer payload. Empty and mixed suffixes fail. The loop is finite and does
/// not claim crash-safe or cross-process atomic replacement.
#[test]
fn concurrent_writes_and_reads_preserve_payloads() {
    const ROUNDS: usize = 64;
    const FILE: &str = "shared.bin";
    const DEADLINE: Duration = Duration::from_secs(20);
    let initial = vec![0x11u8; 32 * 1024 + 64];
    let payload_a = vec![0xA5u8; 32 * 1024];
    let payload_b = vec![0x5Au8; 32 * 1024 - 128];
    let expected: [&[u8]; 3] = [&initial, &payload_a, &payload_b];

    let workspace = Arc::new(Workspace::new(64 * 1024).unwrap());
    workspace.write_file(FILE, &initial).unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let done_a = AtomicBool::new(false);
    let done_b = AtomicBool::new(false);
    let done_a = &done_a;
    let done_b = &done_b;

    std::thread::scope(|scope| {
        let workspace_a = Arc::clone(&workspace);
        let barrier_a = Arc::clone(&barrier);
        let payload_a = payload_a.clone();
        scope.spawn(move || {
            barrier_a.wait();
            let _finished = WriterDone(done_a);
            for _ in 0..ROUNDS {
                workspace_a.write_file(FILE, &payload_a).unwrap();
            }
        });

        let workspace_b = Arc::clone(&workspace);
        let barrier_b = Arc::clone(&barrier);
        let payload_b = payload_b.clone();
        scope.spawn(move || {
            barrier_b.wait();
            let _finished = WriterDone(done_b);
            for _ in 0..ROUNDS {
                workspace_b.write_file(FILE, &payload_b).unwrap();
            }
        });

        let workspace = Arc::clone(&workspace);
        let barrier = Arc::clone(&barrier);
        scope.spawn(move || {
            barrier.wait();
            let started = Instant::now();
            loop {
                let bytes = workspace.read_file(FILE).expect("read shared file");
                assert!(
                    expected.iter().any(|payload| *payload == bytes),
                    "read {} bytes, not a complete payload (empty or mixed suffix); head {:02x?} tail {:02x?}",
                    bytes.len(),
                    &bytes[..bytes.len().min(8)],
                    &bytes[bytes.len().saturating_sub(8)..],
                );
                if done_a.load(Ordering::Acquire) && done_b.load(Ordering::Acquire) {
                    break;
                }
                assert!(
                    started.elapsed() < DEADLINE,
                    "writers still running after {DEADLINE:?}"
                );
            }
        });
    });

    let settled = workspace.read_file(FILE).unwrap();
    assert!(
        settled == payload_a || settled == payload_b,
        "settled file is not a complete writer payload (len {})",
        settled.len()
    );
}

struct WriterDone<'a>(&'a AtomicBool);

impl Drop for WriterDone<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[test]
fn path_reject_reason_display_names_every_variant() {
    assert_eq!(PathRejectReason::Empty.to_string(), "empty");
    assert_eq!(PathRejectReason::Absolute.to_string(), "absolute");
    assert_eq!(
        PathRejectReason::ParentComponent.to_string(),
        "parent component"
    );
    assert_eq!(
        PathRejectReason::WindowsPrefix.to_string(),
        "Windows prefix"
    );
    assert_eq!(PathRejectReason::Backslash.to_string(), "backslash");
}

fn try_symlink(target: &Path, link: &Path) {
    if let Err(err) = symlink_file(target, link) {
        if cfg!(windows) && err.kind() == io::ErrorKind::PermissionDenied {
            panic!(
                "create symlink {link:?} -> {target:?} failed: {err}. Windows symlink tests require Developer Mode or SeCreateSymbolicLinkPrivilege; PermissionDenied is a failure, not a skip"
            );
        }
        panic!("create symlink {link:?} -> {target:?}: {err}");
    }
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
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "symlinks are not supported on this platform",
        ))
    }
}
