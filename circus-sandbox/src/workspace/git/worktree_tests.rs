use super::*;

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
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

fn options(root: &std::path::Path) -> GitOptions {
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
    std::fs::write(repo.path().join("code.txt"), b"original").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "original"]);
    let revision = git(repo.path(), &["rev-parse", "HEAD"]);
    (repo, revision)
}

#[tokio::test]
async fn dirty_worktree_is_force_removed_without_touching_siblings_or_primary() {
    let (repo, revision) = repository();
    let storage = tempfile::tempdir().unwrap();
    let mut checkout = GitCheckout::clone(
        GitSource::local(repo.path().to_owned()).unwrap(),
        GitRevision::parse(&revision).unwrap(),
        options(storage.path()),
    )
    .await
    .unwrap();
    let mut first = checkout.create_worktree().await.unwrap();
    let mut second = checkout.create_worktree().await.unwrap();
    std::fs::write(first.tree.join("code.txt"), b"modified in the worktree").unwrap();
    std::fs::write(first.tree.join("untracked.txt"), b"untracked").unwrap();

    first.cleanup().await.unwrap();
    // The sibling still copies its own fixed commit, not the dirty modification.
    let mut sibling = second
        .copy_to_workspace(1024, SnapshotBudget::new(1024, 16).unwrap())
        .await
        .unwrap();
    assert_eq!(sibling.read_file("code.txt").unwrap(), b"original");
    assert!(sibling.read_file("untracked.txt").is_err());
    sibling.cleanup().unwrap();
    second.cleanup().await.unwrap();

    // The primary checkout and the source repository are untouched.
    let mut primary = checkout
        .copy_to_workspace(1024, SnapshotBudget::new(1024, 16).unwrap())
        .await
        .unwrap();
    assert_eq!(primary.read_file("code.txt").unwrap(), b"original");
    primary.cleanup().unwrap();
    assert_eq!(
        std::fs::read(repo.path().join("code.txt")).unwrap(),
        b"original"
    );
    checkout.cleanup().await.unwrap();
    assert_eq!(std::fs::read_dir(storage.path()).unwrap().count(), 0);
}
