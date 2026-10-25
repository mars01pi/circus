use circus_sandbox::{GitCheckout, GitOptions, GitRevision, GitSource, SnapshotBudget};
use std::fs;
use std::process::Command;
use std::time::Duration;

fn git(repo: &std::path::Path, args: &[&str]) -> String {
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
fn options(root: &std::path::Path) -> GitOptions {
    GitOptions::new(
        std::path::PathBuf::from(if cfg!(windows) {
            "C:/Program Files/Git/cmd/git.exe"
        } else {
            "/usr/bin/git"
        }),
        root.to_owned(),
        Duration::from_secs(30),
        1 << 20,
    )
    .unwrap()
}

#[tokio::test]
async fn prepares_fixed_old_commit_and_code_only_independent_copy() {
    let repo = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(
        repo.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git(repo.path(), &["config", "user.name", "Test"]);
    fs::write(repo.path().join("value.txt"), b"old").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "old"]);
    let old = git(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(repo.path().join("value.txt"), b"new").unwrap();
    git(repo.path(), &["commit", "-qam", "new"]);
    let source = GitSource::local(repo.path().to_path_buf()).unwrap();
    let mut checkout = GitCheckout::clone(
        source,
        GitRevision::parse(&old).unwrap(),
        options(storage.path()),
    )
    .await
    .unwrap();
    let mut workspace = checkout
        .copy_to_workspace(1024, SnapshotBudget::new(1024, 8).unwrap())
        .await
        .unwrap();
    assert_eq!(workspace.read_file("value.txt").unwrap(), b"old");
    assert!(workspace.read_file(".git/HEAD").is_err());
    for private in ["home", "private-home", "config", "empty-hooks", "tmp"] {
        assert!(workspace.read_file(format!("{private}/marker")).is_err());
    }
    let mut independent = checkout
        .copy_to_workspace(1024, SnapshotBudget::new(1024, 1).unwrap())
        .await
        .unwrap();
    assert_eq!(independent.read_file("value.txt").unwrap(), b"old");
    assert!(
        checkout
            .copy_to_workspace(1024, SnapshotBudget::new(2, 1).unwrap())
            .await
            .is_err()
    );
    independent.cleanup().unwrap();
    workspace.write_file("value.txt", b"copy").unwrap();
    assert_eq!(fs::read(repo.path().join("value.txt")).unwrap(), b"new");
    workspace.cleanup().unwrap();
    checkout.cleanup().await.unwrap();
    checkout.cleanup().await.unwrap();
    assert_eq!(fs::read_dir(storage.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn rejects_invalid_options_and_revision() {
    assert!(GitRevision::parse("HEAD").is_err());
    assert!(
        GitOptions::new(
            "relative-git".into(),
            std::env::temp_dir(),
            Duration::from_secs(1),
            1
        )
        .is_err()
    );
    assert!(
        GitSource::https(url::Url::parse("https://user:pass@example.com/repo.git").unwrap())
            .is_err()
    );
    assert!(GitSource::https(url::Url::parse("http://example.com/repo.git").unwrap()).is_err());
}

fn repository() -> (tempfile::TempDir, String) {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(
        repo.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git(repo.path(), &["config", "user.name", "Test"]);
    fs::write(repo.path().join("code.txt"), b"real code").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "code"]);
    let revision = git(repo.path(), &["rev-parse", "HEAD"]);
    (repo, revision)
}

#[tokio::test]
async fn failed_revision_and_output_budget_remove_private_checkout() {
    let (repo, revision) = repository();
    let storage = tempfile::tempdir().unwrap();
    let error = GitCheckout::clone(
        GitSource::local(repo.path().to_owned()).unwrap(),
        GitRevision::parse(&"0".repeat(40)).unwrap(),
        options(storage.path()),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, circus_sandbox::GitError::Command { .. }));
    assert_eq!(fs::read_dir(storage.path()).unwrap().count(), 0);
    let small = GitOptions::new(
        if cfg!(windows) {
            "C:/Program Files/Git/cmd/git.exe".into()
        } else {
            "/usr/bin/git".into()
        },
        storage.path().to_owned(),
        Duration::from_secs(30),
        1,
    )
    .unwrap();
    let error = GitCheckout::clone(
        GitSource::local(repo.path().to_owned()).unwrap(),
        GitRevision::parse(&revision).unwrap(),
        small,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        circus_sandbox::GitError::Process(
            circus_sandbox::runtime::ProcessError::OutputLimit { .. }
        )
    ));
    assert_eq!(fs::read_dir(storage.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn committed_symlink_is_rejected_by_code_copy() {
    let (repo, _) = repository();
    std::os::unix::fs::symlink("code.txt", repo.path().join("link.txt")).unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "link"]);
    let revision = git(repo.path(), &["rev-parse", "HEAD"]);
    let storage = tempfile::tempdir().unwrap();
    let mut checkout = GitCheckout::clone(
        GitSource::local(repo.path().to_owned()).unwrap(),
        GitRevision::parse(&revision).unwrap(),
        options(storage.path()),
    )
    .await
    .unwrap();
    assert!(matches!(
        checkout
            .copy_to_workspace(1024, SnapshotBudget::new(1024, 10).unwrap())
            .await,
        Err(circus_sandbox::GitError::Copy(_))
    ));
    checkout.cleanup().await.unwrap();
}

#[tokio::test]
async fn dropped_checkout_retains_owner_until_directory_is_removed() {
    let (repo, revision) = repository();
    let storage = tempfile::tempdir().unwrap();
    let checkout = GitCheckout::clone(
        GitSource::local(repo.path().to_owned()).unwrap(),
        GitRevision::parse(&revision).unwrap(),
        options(storage.path()),
    )
    .await
    .unwrap();
    assert_eq!(fs::read_dir(storage.path()).unwrap().count(), 1);
    drop(checkout);
    tokio::time::timeout(Duration::from_secs(10), async {
        while fs::read_dir(storage.path()).unwrap().count() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
