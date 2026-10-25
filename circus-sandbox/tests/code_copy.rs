use std::fs;
use std::path::Path;

use circus_sandbox::{CodeCopyError, CodeSource, SnapshotBudget, WorkspaceError};

#[test]
fn copies_nested_content_into_two_independent_workspaces_without_changing_source() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src/empty")).unwrap();
    fs::write(directory.path().join("src/main.rs"), b"fn main() {}\0").unwrap();
    let source = CodeSource::open(directory.path()).unwrap();
    let budget = SnapshotBudget::new(13, 3).unwrap();
    let mut first = source.copy_to_workspace(13, budget).unwrap();
    let mut second = source.copy_to_workspace(13, budget).unwrap();

    assert_eq!(first.read_file("src/main.rs").unwrap(), b"fn main() {}\0");
    first.write_file("src/main.rs", b"changed").unwrap();
    first.write_file("src/empty/new.txt", b"new").unwrap();
    assert_eq!(second.read_file("src/main.rs").unwrap(), b"fn main() {}\0");
    assert!(matches!(
        second.read_file("src/empty/new.txt"),
        Err(WorkspaceError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
    ));
    second.write_file("src/empty/other.txt", b"other").unwrap();
    assert_eq!(
        fs::read(directory.path().join("src/main.rs")).unwrap(),
        b"fn main() {}\0"
    );
    assert_eq!(
        fs::read_dir(directory.path().join("src/empty"))
            .unwrap()
            .count(),
        0
    );

    first.cleanup().unwrap();
    drop(source);
    directory.close().unwrap();
    assert_eq!(second.read_file("src/main.rs").unwrap(), b"fn main() {}\0");
    second.cleanup().unwrap();
}

#[test]
fn accepts_exact_file_and_total_limits_and_rejects_one_byte_over_each() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("a"), b"1234").unwrap();
    fs::write(directory.path().join("b"), b"5678").unwrap();
    let source = CodeSource::open(directory.path()).unwrap();
    let mut exact = source
        .copy_to_workspace(4, SnapshotBudget::new(8, 2).unwrap())
        .unwrap();
    assert_eq!(exact.read_file("a").unwrap(), b"1234");
    assert_eq!(exact.read_file("b").unwrap(), b"5678");
    exact.cleanup().unwrap();

    assert!(matches!(
        source.copy_to_workspace(3, SnapshotBudget::new(8, 2).unwrap()),
        Err(CodeCopyError::Workspace(WorkspaceError::LimitExceeded {
            limit: 3,
            actual: 4
        }))
    ));
    assert!(matches!(
        source.copy_to_workspace(4, SnapshotBudget::new(7, 2).unwrap()),
        Err(CodeCopyError::ByteLimit {
            limit: 7,
            actual: 8
        })
    ));
    assert_eq!(fs::read(directory.path().join("a")).unwrap(), b"1234");
    assert_eq!(fs::read(directory.path().join("b")).unwrap(), b"5678");
    drop(source);
    directory.close().unwrap();
}

#[test]
fn counts_empty_directories_and_zero_byte_files_but_not_root() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("nested/empty")).unwrap();
    fs::write(directory.path().join("zero"), b"").unwrap();
    let source = CodeSource::open(directory.path()).unwrap();
    let mut exact = source
        .copy_to_workspace(1, SnapshotBudget::new(1, 3).unwrap())
        .unwrap();
    assert_eq!(exact.read_file("zero").unwrap(), b"");
    exact.write_file("nested/empty/child", b"x").unwrap();
    assert!(matches!(
        source.copy_to_workspace(1, SnapshotBudget::new(1, 2).unwrap()),
        Err(CodeCopyError::EntryLimit {
            limit: 2,
            actual: 3
        })
    ));
    exact.cleanup().unwrap();
    drop(source);
    directory.close().unwrap();
}

#[test]
fn rejects_invalid_destination_file_limits() {
    let directory = tempfile::tempdir().unwrap();
    let source = CodeSource::open(directory.path()).unwrap();
    for limit in [0, usize::MAX] {
        assert!(matches!(
            source.copy_to_workspace(limit, SnapshotBudget::new(1, 1).unwrap()),
            Err(CodeCopyError::Workspace(WorkspaceError::InvalidLimit { max_file_bytes }))
                if max_file_bytes == limit
        ));
    }
    drop(source);
    directory.close().unwrap();
}

#[test]
fn copies_an_empty_source_and_applies_budget_only_to_the_copy() {
    let directory = tempfile::tempdir().unwrap();
    let source = CodeSource::open(directory.path()).unwrap();
    let mut workspace = source
        .copy_to_workspace(4, SnapshotBudget::new(1, 1).unwrap())
        .unwrap();
    // The import budget does not silently become a future workspace quota.
    workspace.write_file("one", b"1234").unwrap();
    workspace.write_file("two", b"5678").unwrap();
    assert_eq!(workspace.read_file("one").unwrap(), b"1234");
    assert_eq!(workspace.read_file("two").unwrap(), b"5678");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    workspace.cleanup().unwrap();
    drop(source);
    directory.close().unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn rejects_a_file_symlink_escaping_the_authorized_source() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret");
    fs::write(&secret, b"not granted").unwrap();
    let link = directory.path().join("escape");
    symlink_file(&secret, &link);
    let source = CodeSource::open(directory.path()).unwrap();
    assert!(matches!(
        source.copy_to_workspace(32, SnapshotBudget::new(32, 4).unwrap()),
        Err(CodeCopyError::Symlink { path }) if path == Path::new("escape")
    ));
    assert_eq!(fs::read(&secret).unwrap(), b"not granted");
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    drop(source);
    directory.close().unwrap();
    outside.close().unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn rejects_internal_links_and_directory_links_instead_of_following_them() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("target"), b"bytes").unwrap();
    symlink_file(Path::new("target"), &directory.path().join("alias"));
    let source = CodeSource::open(directory.path()).unwrap();
    assert!(matches!(
        source.copy_to_workspace(32, SnapshotBudget::new(32, 8).unwrap()),
        Err(CodeCopyError::Symlink { path }) if path == Path::new("alias")
    ));
    drop(source);
    fs::remove_file(directory.path().join("alias")).unwrap();
    fs::create_dir(directory.path().join("real_dir")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("real_dir", directory.path().join("alias_dir")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir("real_dir", directory.path().join("alias_dir"))
        .expect("Windows symlink tests require Developer Mode or SeCreateSymbolicLinkPrivilege; permission failure is not a skip");
    let source = CodeSource::open(directory.path()).unwrap();
    assert!(matches!(
        source.copy_to_workspace(32, SnapshotBudget::new(32, 8).unwrap()),
        Err(CodeCopyError::Symlink { path }) if path == Path::new("alias_dir")
    ));
    drop(source);
    directory.close().unwrap();
}

#[cfg(unix)]
#[test]
fn rejects_fifo_and_socket_without_reading_them() {
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("spawn mkfifo for non-regular source test");
    assert!(status.success(), "mkfifo exited {status}");
    let source = CodeSource::open(directory.path()).unwrap();
    assert!(matches!(
        source.copy_to_workspace(32, SnapshotBudget::new(32, 4).unwrap()),
        Err(CodeCopyError::NotRegular { path }) if path == Path::new("pipe")
    ));
    drop(source);
    fs::remove_file(fifo).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(directory.path().join("socket")).unwrap();
    let source = CodeSource::open(directory.path()).unwrap();
    assert!(matches!(
        source.copy_to_workspace(32, SnapshotBudget::new(32, 4).unwrap()),
        Err(CodeCopyError::NotRegular { path }) if path == Path::new("socket")
    ));
    drop(source);
    drop(listener);
    directory.close().unwrap();
}

#[cfg(unix)]
#[test]
fn rejects_nonportable_source_names() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    for name in [
        OsString::from("a\\b"),
        OsString::from("C:drive"),
        OsString::from_vec(vec![0xff]),
    ] {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join(&name), b"bytes").unwrap();
        let source = CodeSource::open(directory.path()).unwrap();
        assert!(matches!(
            source.copy_to_workspace(32, SnapshotBudget::new(32, 4).unwrap()),
            Err(CodeCopyError::NotPortable { path }) if path == Path::new(&name)
        ));
        assert_eq!(fs::read(directory.path().join(&name)).unwrap(), b"bytes");
        drop(source);
        directory.close().unwrap();
    }
}

#[cfg(any(unix, windows))]
fn symlink_file(target: &Path, link: &Path) {
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_file(target, link);
    result.expect("create source symlink; Windows requires Developer Mode or SeCreateSymbolicLinkPrivilege; permission failure is not a skip");
}
