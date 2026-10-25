use circus_sandbox::{GitCheckout, GitOptions, GitRevision, GitSource, SnapshotBudget};
use std::fs;
use std::process::Command;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let repo = tempfile::tempdir()?;
    let run = |args: &[&str]| -> Result<String, Box<dyn std::error::Error>> {
        let out = Command::new("git")
            .current_dir(repo.path())
            .args(args)
            .output()?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned().into());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    run(&["init", "-q"])?;
    run(&["config", "user.email", "example@example.invalid"])?;
    run(&["config", "user.name", "Example"])?;
    fs::write(
        repo.path().join("hello.txt"),
        b"hello from a fixed Git worktree\n",
    )?;
    run(&["add", "."])?;
    run(&["commit", "-qm", "example"])?;
    let revision = run(&["rev-parse", "HEAD"])?;

    let storage = tempfile::tempdir()?;
    let executable = std::path::PathBuf::from(if cfg!(windows) {
        "C:/Program Files/Git/cmd/git.exe"
    } else {
        "/usr/bin/git"
    });
    let options = GitOptions::new(
        executable,
        storage.path().to_path_buf(),
        Duration::from_secs(30),
        1 << 20,
    )?;
    let mut checkout = GitCheckout::clone(
        GitSource::local(repo.path().to_path_buf())?,
        GitRevision::parse(&revision)?,
        options,
    )
    .await?;

    let mut first = checkout.create_worktree().await?;
    let mut second = checkout.create_worktree().await?;
    let expected = GitRevision::parse(&revision)?;
    println!("checkout revision: {revision}");
    println!(
        "checkout revision matches source: {}",
        checkout.revision() == &expected
    );
    println!(
        "first worktree revision matches: {}",
        first.revision() == &expected
    );
    println!(
        "second worktree revision matches: {}",
        second.revision() == &expected
    );
    let budget = SnapshotBudget::new(1024, 8)?;
    let mut first_copy = first.copy_to_workspace(1024, budget).await?;
    let mut second_copy = second.copy_to_workspace(1024, budget).await?;
    let first_bytes = first_copy.read_file("hello.txt")?;
    let second_bytes = second_copy.read_file("hello.txt")?;
    println!(
        "first worktree bytes: {}",
        String::from_utf8_lossy(&first_bytes).trim_end()
    );
    println!(
        "second worktree bytes: {}",
        String::from_utf8_lossy(&second_bytes).trim_end()
    );
    println!(
        "first copy contains .git: {}",
        first_copy.read_file(".git").is_ok()
    );
    first_copy.write_file("hello.txt", b"only this copy\n")?;
    println!(
        "second copy after mutating first: {}",
        String::from_utf8_lossy(&second_copy.read_file("hello.txt")?).trim_end()
    );

    first_copy.cleanup()?;
    second_copy.cleanup()?;
    first.cleanup().await?;
    second.cleanup().await?;
    checkout.cleanup().await?;
    println!(
        "private storage entries after cleanup: {}",
        fs::read_dir(storage.path())?.count()
    );
    Ok(())
}
