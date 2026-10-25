use circus_sandbox::{
    CodeCopyError, GitCheckout, GitError, GitOptions, GitRevision, GitSource, SnapshotBudget,
    WorkspaceError,
};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn options(root: &Path) -> GitOptions {
    let executable = if cfg!(windows) {
        "C:/Program Files/Git/cmd/git.exe"
    } else {
        "/usr/bin/git"
    };
    GitOptions::new(
        executable.into(),
        root.to_owned(),
        Duration::from_secs(30),
        1 << 20,
    )
    .unwrap()
}

fn repository() -> (tempfile::TempDir, String) {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(
        repo.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git(repo.path(), &["config", "user.name", "Test"]);
    fs::write(repo.path().join("code.txt"), b"original").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "original"]);
    let revision = git(repo.path(), &["rev-parse", "HEAD"]);
    (repo, revision)
}

async fn open_checkout(storage: &Path, repo: &Path, revision: &str) -> GitCheckout {
    GitCheckout::clone(
        GitSource::local(repo.to_owned()).unwrap(),
        GitRevision::parse(revision).unwrap(),
        options(storage),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn creates_fixed_commit_worktree_with_independent_code_only_copies() {
    let (repo, revision) = repository();
    let storage = tempfile::tempdir().unwrap();
    let mut checkout = open_checkout(storage.path(), repo.path(), &revision).await;
    // Advance the source repository after the checkout; the worktree must stay
    // pinned to the verified revision rather than follow the source HEAD.
    fs::write(repo.path().join("code.txt"), b"later").unwrap();
    git(repo.path(), &["commit", "-qam", "later"]);
    let mut worktree = checkout.create_worktree().await.unwrap();
    assert_eq!(worktree.revision(), checkout.revision());

    let mut first = worktree
        .copy_to_workspace(1024, SnapshotBudget::new(1024, 16).unwrap())
        .await
        .unwrap();
    assert_eq!(first.read_file("code.txt").unwrap(), b"original");
    assert!(first.read_file(".git").is_err());
    let mut second = worktree
        .copy_to_workspace(1024, SnapshotBudget::new(1024, 16).unwrap())
        .await
        .unwrap();
    first.write_file("code.txt", b"mutated").unwrap();
    assert_eq!(second.read_file("code.txt").unwrap(), b"original");
    first.cleanup().unwrap();
    second.cleanup().unwrap();

    worktree.cleanup().await.unwrap();
    assert_eq!(fs::read_dir(storage.path()).unwrap().count(), 1);
    checkout.cleanup().await.unwrap();
    assert_eq!(fs::read_dir(storage.path()).unwrap().count(), 0);
    assert_eq!(fs::read(repo.path().join("code.txt")).unwrap(), b"later");
}

#[tokio::test]
async fn concurrent_worktrees_are_independently_cleanable() {
    let (repo, revision) = repository();
    let storage = tempfile::tempdir().unwrap();
    let mut checkout = open_checkout(storage.path(), repo.path(), &revision).await;
    let mut first = checkout.create_worktree().await.unwrap();
    let mut second = checkout.create_worktree().await.unwrap();
    let mut first_copy = first
        .copy_to_workspace(1024, SnapshotBudget::new(1024, 16).unwrap())
        .await
        .unwrap();
    let mut second_copy = second
        .copy_to_workspace(1024, SnapshotBudget::new(1024, 16).unwrap())
        .await
        .unwrap();
    first_copy.write_file("code.txt", b"first only").unwrap();
    assert_eq!(second_copy.read_file("code.txt").unwrap(), b"original");
    first_copy.cleanup().unwrap();
    second_copy.cleanup().unwrap();

    first.cleanup().await.unwrap();
    // The sibling worktree and the primary checkout still copy real bytes.
    let mut sibling = second
        .copy_to_workspace(1024, SnapshotBudget::new(1024, 16).unwrap())
        .await
        .unwrap();
    assert_eq!(sibling.read_file("code.txt").unwrap(), b"original");
    let mut primary = checkout
        .copy_to_workspace(1024, SnapshotBudget::new(1024, 16).unwrap())
        .await
        .unwrap();
    assert_eq!(primary.read_file("code.txt").unwrap(), b"original");
    sibling.cleanup().unwrap();
    primary.cleanup().unwrap();

    second.cleanup().await.unwrap();
    checkout.cleanup().await.unwrap();
    assert_eq!(fs::read_dir(storage.path()).unwrap().count(), 0);
    assert_eq!(fs::read(repo.path().join("code.txt")).unwrap(), b"original");
}

#[tokio::test]
async fn worktree_remains_cleanable_after_checkout_cleanup() {
    let (repo, revision) = repository();
    let storage = tempfile::tempdir().unwrap();
    let mut checkout = open_checkout(storage.path(), repo.path(), &revision).await;
    let mut worktree = checkout.create_worktree().await.unwrap();
    checkout.cleanup().await.unwrap();
    // Only the worktree's private root remains; its repository metadata is gone.
    assert_eq!(fs::read_dir(storage.path()).unwrap().count(), 1);
    let mut copy = worktree
        .copy_to_workspace(1024, SnapshotBudget::new(1024, 16).unwrap())
        .await
        .unwrap();
    assert_eq!(copy.read_file("code.txt").unwrap(), b"original");
    copy.cleanup().unwrap();
    worktree.cleanup().await.unwrap();
    assert_eq!(fs::read_dir(storage.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn cleanup_is_idempotent_and_closed_state_rejects_new_work() {
    let (repo, revision) = repository();
    let storage = tempfile::tempdir().unwrap();
    let mut checkout = open_checkout(storage.path(), repo.path(), &revision).await;
    let mut worktree = checkout.create_worktree().await.unwrap();
    worktree.cleanup().await.unwrap();
    worktree.cleanup().await.unwrap();
    assert!(matches!(
        worktree
            .copy_to_workspace(1024, SnapshotBudget::new(1024, 16).unwrap())
            .await,
        Err(GitError::Closed)
    ));

    checkout.cleanup().await.unwrap();
    checkout.cleanup().await.unwrap();
    assert!(matches!(
        checkout.create_worktree().await,
        Err(GitError::Closed)
    ));
    assert!(matches!(
        checkout
            .copy_to_workspace(1024, SnapshotBudget::new(1024, 16).unwrap())
            .await,
        Err(GitError::Closed)
    ));
    assert_eq!(fs::read_dir(storage.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn copy_rejects_byte_entry_and_per_file_budgets() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(
        repo.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git(repo.path(), &["config", "user.name", "Test"]);
    fs::write(repo.path().join("a.txt"), b"12345").unwrap();
    fs::create_dir(repo.path().join("dir")).unwrap();
    fs::write(repo.path().join("dir/b.txt"), b"67890").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "budget"]);
    let revision = git(repo.path(), &["rev-parse", "HEAD"]);
    let storage = tempfile::tempdir().unwrap();
    let mut checkout = open_checkout(storage.path(), repo.path(), &revision).await;
    let mut worktree = checkout.create_worktree().await.unwrap();

    assert!(matches!(
        worktree
            .copy_to_workspace(1024, SnapshotBudget::new(4, 16).unwrap())
            .await,
        Err(GitError::Copy(CodeCopyError::ByteLimit { .. }))
    ));
    assert!(matches!(
        worktree
            .copy_to_workspace(1024, SnapshotBudget::new(1024, 1).unwrap())
            .await,
        Err(GitError::Copy(CodeCopyError::EntryLimit { .. }))
    ));
    assert!(matches!(
        worktree
            .copy_to_workspace(2, SnapshotBudget::new(1024, 16).unwrap())
            .await,
        Err(GitError::Copy(CodeCopyError::Workspace(
            WorkspaceError::LimitExceeded { .. }
        )))
    ));
    let mut valid = worktree
        .copy_to_workspace(1024, SnapshotBudget::new(1024, 16).unwrap())
        .await
        .unwrap();
    assert_eq!(valid.read_file("a.txt").unwrap(), b"12345");
    assert_eq!(valid.read_file("dir/b.txt").unwrap(), b"67890");
    valid.cleanup().unwrap();
    worktree.cleanup().await.unwrap();
    checkout.cleanup().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn worktree_copy_rejects_committed_symlinks() {
    let (repo, _) = repository();
    std::os::unix::fs::symlink("code.txt", repo.path().join("link.txt")).unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "link"]);
    let revision = git(repo.path(), &["rev-parse", "HEAD"]);
    let storage = tempfile::tempdir().unwrap();
    let mut checkout = open_checkout(storage.path(), repo.path(), &revision).await;
    let mut worktree = checkout.create_worktree().await.unwrap();
    assert!(matches!(
        worktree
            .copy_to_workspace(1024, SnapshotBudget::new(1024, 16).unwrap())
            .await,
        Err(GitError::Copy(CodeCopyError::Symlink { .. }))
    ));
    worktree.cleanup().await.unwrap();
    checkout.cleanup().await.unwrap();
}

#[tokio::test]
async fn cancelled_and_dropped_copies_leave_no_owned_worktree_behind() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(
        repo.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git(repo.path(), &["config", "user.name", "Test"]);
    for index in 0..64 {
        fs::write(
            repo.path().join(format!("file-{index}.txt")),
            vec![b'x'; 1024],
        )
        .unwrap();
    }
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "many"]);
    let revision = git(repo.path(), &["rev-parse", "HEAD"]);
    let storage = tempfile::tempdir().unwrap();
    let mut checkout = open_checkout(storage.path(), repo.path(), &revision).await;

    let worktree = checkout.create_worktree().await.unwrap();
    let copy = tokio::time::timeout(
        Duration::from_nanos(1),
        worktree.copy_to_workspace(4096, SnapshotBudget::new(1 << 20, 512).unwrap()),
    )
    .await;
    if let Ok(Ok(mut workspace)) = copy {
        workspace.cleanup().unwrap();
    }
    // The owner must still accept work after draining a cancelled copy.
    let mut recovered = worktree
        .copy_to_workspace(4096, SnapshotBudget::new(1 << 20, 512).unwrap())
        .await
        .unwrap();
    assert_eq!(recovered.read_file("file-0.txt").unwrap().len(), 1024);
    recovered.cleanup().unwrap();

    // Dropping the handle while a copy is in flight cancels and cleans it; the
    // private worktree directory is removed by the retained owner.
    let handle = tokio::spawn(async move {
        let _copy = worktree
            .copy_to_workspace(4096, SnapshotBudget::new(1 << 20, 512).unwrap())
            .await;
        worktree
    });
    tokio::task::yield_now().await;
    handle.abort();
    let _ = handle.await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while fs::read_dir(storage.path()).unwrap().count() != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    checkout.cleanup().await.unwrap();
    assert_eq!(fs::read_dir(storage.path()).unwrap().count(), 0);
}
