use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{
    CodeSnapshot, SnapshotBody, SnapshotBudget, SnapshotEntry, SnapshotError, Workspace,
    WorkspaceError,
};

const MAGIC: [u8; 8] = *b"CRCSNAP\0";

fn budget() -> SnapshotBudget {
    SnapshotBudget::new(64, 16).unwrap()
}

#[test]
fn roundtrip_preserves_empty_directories_empty_files_and_unicode() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("snapshot.bin");
    let snapshot = sample_snapshot();
    snapshot.save_file(&path).unwrap();

    let copy = directory.path().join("copy.bin");
    snapshot.save_file(&copy).unwrap();
    assert_eq!(fs::read(&path).unwrap(), fs::read(&copy).unwrap());

    let loaded = CodeSnapshot::load_file(&path, 64, budget()).unwrap();
    assert_eq!(loaded.entry_count(), snapshot.entry_count());
    assert_eq!(loaded.total_bytes(), snapshot.total_bytes());
    assert_eq!(loaded.max_file_bytes(), 64);

    let mut restored = loaded.restore().unwrap();
    assert_eq!(restored.read_file("hello.txt").unwrap(), b"hello");
    assert_eq!(
        restored.read_file("目录/é.txt").unwrap(),
        "héllo".as_bytes()
    );
    assert_eq!(restored.read_file("empty.bin").unwrap(), b"");
    let root = restored.temp.as_ref().unwrap().path();
    assert!(root.join("empty").is_dir());
    restored.cleanup().unwrap();
}

#[test]
fn save_is_deterministic_across_entry_order() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.bin");
    let second = directory.path().join("second.bin");
    let entries = || {
        vec![
            SnapshotEntry {
                path: PathBuf::from("a"),
                body: SnapshotBody::File(b"x".to_vec()),
            },
            SnapshotEntry {
                path: PathBuf::from("d"),
                body: SnapshotBody::Directory,
            },
        ]
    };
    CodeSnapshot::from_entries(8, entries())
        .unwrap()
        .save_file(&first)
        .unwrap();
    let mut reversed = entries();
    reversed.reverse();
    CodeSnapshot::from_entries(8, reversed)
        .unwrap()
        .save_file(&second)
        .unwrap();
    assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
}

#[test]
fn empty_snapshot_roundtrips() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("empty.bin");
    let mut workspace = Workspace::new(8).unwrap();
    let snapshot = workspace
        .capture_code_snapshot(SnapshotBudget::new(1, 1).unwrap())
        .unwrap();
    snapshot.save_file(&path).unwrap();
    let loaded = CodeSnapshot::load_file(&path, 8, SnapshotBudget::new(1, 1).unwrap()).unwrap();
    assert_eq!(loaded.entry_count(), 0);
    assert_eq!(loaded.total_bytes(), 0);
    let mut restored = loaded.restore().unwrap();
    restored.cleanup().unwrap();
    workspace.cleanup().unwrap();
}

#[test]
fn exact_budgets_are_allowed_and_one_less_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("budget.bin");
    let mut workspace = Workspace::new(8).unwrap();
    workspace.create_dir_all("d").unwrap();
    workspace.write_file("d/a", b"12345678").unwrap();
    let snapshot = workspace
        .capture_code_snapshot(SnapshotBudget::new(8, 2).unwrap())
        .unwrap();
    snapshot.save_file(&path).unwrap();

    CodeSnapshot::load_file(&path, 8, SnapshotBudget::new(8, 2).unwrap()).unwrap();
    assert!(matches!(
        CodeSnapshot::load_file(&path, 8, SnapshotBudget::new(8, 1).unwrap()),
        Err(SnapshotError::EntryLimit {
            limit: 1,
            actual: 2
        })
    ));
    assert!(matches!(
        CodeSnapshot::load_file(&path, 8, SnapshotBudget::new(7, 2).unwrap()),
        Err(SnapshotError::ByteLimit {
            limit: 7,
            actual: 8
        })
    ));
    assert!(matches!(
        CodeSnapshot::load_file(&path, 7, SnapshotBudget::new(8, 2).unwrap()),
        Err(SnapshotError::Workspace(WorkspaceError::LimitExceeded {
            limit: 7,
            actual: 8
        }))
    ));
    workspace.cleanup().unwrap();
}

#[test]
fn load_rejects_header_damage() {
    let directory = tempfile::tempdir().unwrap();

    let mut bad_magic = header(0, 0);
    bad_magic[0] = b'X';
    assert_invalid(&write_archive(&directory, "magic.bin", &bad_magic));

    let mut bad_version = header(0, 0);
    bad_version[8..10].copy_from_slice(&2u16.to_le_bytes());
    assert_invalid(&write_archive(&directory, "version.bin", &bad_version));

    let mut bad_flags = header(0, 0);
    bad_flags[10..12].copy_from_slice(&1u16.to_le_bytes());
    assert_invalid(&write_archive(&directory, "flags.bin", &bad_flags));

    let truncated = header(0, 0);
    assert_invalid(&write_archive(&directory, "header.bin", &truncated[..12]));

    let mut trailing = header(0, 0);
    trailing.push(0xff);
    assert_invalid(&write_archive(&directory, "trailing.bin", &trailing));

    assert_invalid(&write_archive(&directory, "total.bin", &header(0, 1)));
}

#[test]
fn load_rejects_unknown_kind_and_non_utf8_path() {
    let directory = tempfile::tempdir().unwrap();

    let mut unknown = header(1, 0);
    unknown.extend([9u8, 0, 0, 0, 0, 0, 0, 0]);
    assert_invalid(&write_archive(&directory, "kind.bin", &unknown));

    let mut non_utf8 = header(1, 0);
    non_utf8.push(0);
    non_utf8.extend(1u32.to_le_bytes());
    non_utf8.push(0xff);
    assert_invalid(&write_archive(&directory, "utf8.bin", &non_utf8));
}

#[test]
fn load_rejects_nonportable_and_duplicate_paths() {
    let directory = tempfile::tempdir().unwrap();
    for (name, path) in [
        ("absolute.bin", "/tmp/x"),
        ("parent.bin", "../x"),
        ("backslash.bin", "a\\b"),
        ("drive.bin", "C:/x"),
        ("unc.bin", "//server/share"),
        ("dot.bin", "."),
        ("alias.bin", "a//b"),
    ] {
        let mut archive = header(1, 0);
        push_directory(&mut archive, path);
        assert_invalid(&write_archive(&directory, name, &archive));
    }

    let mut duplicate = header(2, 0);
    push_directory(&mut duplicate, "a");
    push_directory(&mut duplicate, "a");
    assert_invalid(&write_archive(&directory, "duplicate.bin", &duplicate));

    let mut unsorted = header(2, 0);
    push_directory(&mut unsorted, "b");
    push_directory(&mut unsorted, "a");
    assert_invalid(&write_archive(&directory, "unsorted.bin", &unsorted));
}

#[test]
fn load_rejects_oversized_path_and_forged_counts_without_large_allocation() {
    let directory = tempfile::tempdir().unwrap();

    let mut oversized = header(1, 0);
    oversized.push(0);
    oversized.extend(4097u32.to_le_bytes());
    oversized.push(0);
    assert_invalid(&write_archive(&directory, "path.bin", &oversized));

    let huge_count = header(u64::MAX, 0);
    let path = write_archive(&directory, "count.bin", &huge_count);
    let budget = SnapshotBudget::new(1, usize::MAX).unwrap();
    assert_invalid_with(&path, 16, budget);

    let mut huge_file = header(1, 12);
    huge_file.push(1);
    huge_file.extend(1u32.to_le_bytes());
    huge_file.push(b'a');
    huge_file.extend(100u64.to_le_bytes());
    huge_file.extend([0u8; 12]);
    let path = write_archive(&directory, "file.bin", &huge_file);
    assert!(matches!(
        CodeSnapshot::load_file(&path, 16, SnapshotBudget::new(12, 4).unwrap()),
        Err(SnapshotError::Workspace(WorkspaceError::LimitExceeded {
            limit: 16,
            actual: 100
        }))
    ));
}

#[test]
fn load_rejects_truncated_payload_and_independent_totals() {
    let directory = tempfile::tempdir().unwrap();

    let mut archive = header(1, 3);
    push_file(&mut archive, "a", b"xyz");
    assert_invalid(&write_archive(
        &directory,
        "truncated.bin",
        &archive[..archive.len() - 1],
    ));

    let mut missing = header(1, 5);
    push_file(&mut missing, "a", b"xy");
    assert_invalid(&write_archive(&directory, "missing.bin", &missing));

    let mut mismatch = header(1, 5);
    push_directory(&mut mismatch, "a");
    assert_invalid(&write_archive(&directory, "mismatch.bin", &mismatch));
}

#[test]
fn save_validates_paths_and_duplicates_before_touching_the_target() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("snapshot.bin");
    let entries = vec![SnapshotEntry {
        path: PathBuf::from(".."),
        body: SnapshotBody::File(Vec::new()),
    }];
    let snapshot = CodeSnapshot::from_entries(8, entries).unwrap();
    assert!(matches!(
        snapshot.save_file(&path),
        Err(SnapshotError::NotPortable { .. })
    ));
    assert!(!path.exists());

    let entries = vec![SnapshotEntry {
        path: PathBuf::from("a//b"),
        body: SnapshotBody::File(Vec::new()),
    }];
    let snapshot = CodeSnapshot::from_entries(8, entries).unwrap();
    assert!(matches!(
        snapshot.save_file(&path),
        Err(SnapshotError::InvalidArchive { .. })
    ));
    assert!(!path.exists());

    let entries = vec![
        SnapshotEntry {
            path: PathBuf::from("a"),
            body: SnapshotBody::Directory,
        },
        SnapshotEntry {
            path: PathBuf::from("a"),
            body: SnapshotBody::File(b"x".to_vec()),
        },
    ];
    let snapshot = CodeSnapshot::from_entries(8, entries).unwrap();
    assert!(matches!(
        snapshot.save_file(&path),
        Err(SnapshotError::InvalidArchive { .. })
    ));
    assert!(!path.exists());
    assert_no_temporary_file(directory.path());
}

#[test]
fn overwrite_replaces_archive_and_failed_save_leaves_no_temporary_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("snapshot.bin");

    let first = snapshot_with_file("first.txt", b"one");
    first.save_file(&path).unwrap();
    let second = snapshot_with_file("second.txt", b"two");
    second.save_file(&path).unwrap();
    assert_no_temporary_file(directory.path());

    let loaded = CodeSnapshot::load_file(&path, 8, SnapshotBudget::new(8, 2).unwrap()).unwrap();
    let mut restored = loaded.restore().unwrap();
    assert_eq!(restored.read_file("second.txt").unwrap(), b"two");
    restored.cleanup().unwrap();

    let blocked = directory.path().join("blocked");
    fs::create_dir(&blocked).unwrap();
    assert!(matches!(
        second.save_file(&blocked),
        Err(SnapshotError::Io {
            operation: "rename",
            ..
        })
    ));
    assert_no_temporary_file(directory.path());
}

#[test]
fn save_replaces_symlink_entry_instead_of_writing_through_it() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target");
    fs::write(&target, b"keep").unwrap();
    let link = directory.path().join("archive");
    symlink_file(&target, &link).unwrap();

    let snapshot = snapshot_with_file("value", b"new");
    snapshot.save_file(&link).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"keep");
    assert!(
        !fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    CodeSnapshot::load_file(&link, 8, SnapshotBudget::new(8, 2).unwrap()).unwrap();
}

#[test]
fn load_rejects_symlink_and_special_archive_paths() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("archive");
    snapshot_with_file("value", b"new")
        .save_file(&archive)
        .unwrap();
    let link = directory.path().join("link");
    symlink_file(&archive, &link).unwrap();

    assert!(matches!(
        CodeSnapshot::load_file(&link, 8, SnapshotBudget::new(8, 2).unwrap()),
        Err(SnapshotError::Io {
            operation: "inspect",
            ..
        })
    ));
    assert!(matches!(
        CodeSnapshot::load_file(directory.path(), 8, SnapshotBudget::new(8, 2).unwrap()),
        Err(SnapshotError::Io {
            operation: "inspect",
            ..
        })
    ));
}

fn sample_snapshot() -> CodeSnapshot {
    let mut workspace = Workspace::new(64).unwrap();
    workspace.create_dir_all("empty").unwrap();
    workspace.create_dir_all("目录").unwrap();
    workspace.write_file("hello.txt", b"hello").unwrap();
    workspace
        .write_file("目录/é.txt", "héllo".as_bytes())
        .unwrap();
    workspace.write_file("empty.bin", b"").unwrap();
    let snapshot = workspace
        .capture_code_snapshot(SnapshotBudget::new(64, 16).unwrap())
        .unwrap();
    workspace.cleanup().unwrap();
    snapshot
}

fn snapshot_with_file(name: &str, contents: &[u8]) -> CodeSnapshot {
    let mut workspace = Workspace::new(8).unwrap();
    workspace.write_file(name, contents).unwrap();
    let snapshot = workspace
        .capture_code_snapshot(SnapshotBudget::new(8, 2).unwrap())
        .unwrap();
    workspace.cleanup().unwrap();
    snapshot
}

fn header(entry_count: u64, total_bytes: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend(MAGIC);
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(0u16.to_le_bytes());
    bytes.extend(entry_count.to_le_bytes());
    bytes.extend(total_bytes.to_le_bytes());
    bytes
}

fn push_directory(bytes: &mut Vec<u8>, path: &str) {
    bytes.push(0);
    bytes.extend(u32::try_from(path.len()).unwrap().to_le_bytes());
    bytes.extend(path.as_bytes());
}

fn push_file(bytes: &mut Vec<u8>, path: &str, contents: &[u8]) {
    bytes.push(1);
    bytes.extend(u32::try_from(path.len()).unwrap().to_le_bytes());
    bytes.extend(path.as_bytes());
    bytes.extend(u64::try_from(contents.len()).unwrap().to_le_bytes());
    bytes.extend(contents);
}

fn write_archive(directory: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = directory.path().join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn assert_invalid(path: &Path) {
    assert_invalid_with(path, 16, SnapshotBudget::new(64, 16).unwrap());
}

fn assert_invalid_with(path: &Path, max_file_bytes: usize, budget: SnapshotBudget) {
    assert!(
        matches!(
            CodeSnapshot::load_file(path, max_file_bytes, budget),
            Err(SnapshotError::InvalidArchive { .. })
                | Err(SnapshotError::NotPortable { .. })
                | Err(SnapshotError::EntryLimit { .. })
        ),
        "expected a typed format rejection"
    );
}

fn assert_no_temporary_file(directory: &Path) {
    for entry in fs::read_dir(directory).unwrap() {
        let name = entry.unwrap().file_name();
        assert!(
            !name.to_string_lossy().starts_with(".circus-snapshot-"),
            "temporary archive was left behind: {name:?}"
        );
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
}
