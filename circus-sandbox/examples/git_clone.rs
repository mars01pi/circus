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
    fs::write(repo.path().join("hello.txt"), b"hello from fixed Git\n")?;
    run(&["add", "."])?;
    run(&["commit", "-qm", "example"])?;
    let revision = run(&["rev-parse", "HEAD"])?;
    let storage = tempfile::tempdir()?;
    let executable = std::path::PathBuf::from(if cfg!(windows) {
        "C:/Program Files/Git/cmd/git.exe"
    } else {
        "/usr/bin/git"
    });
    let source = GitSource::local(repo.path().to_path_buf())?;
    let mut checkout = GitCheckout::clone(
        source,
        GitRevision::parse(&revision)?,
        GitOptions::new(
            executable,
            storage.path().to_path_buf(),
            Duration::from_secs(30),
            1 << 20,
        )?,
    )
    .await?;
    let mut workspace = checkout
        .copy_to_workspace(1024, SnapshotBudget::new(1024, 8)?)
        .await?;
    let bytes = workspace.read_file("hello.txt")?;
    assert_eq!(bytes, b"hello from fixed Git\n");
    println!("{}", String::from_utf8_lossy(&bytes));
    workspace.cleanup()?;
    checkout.cleanup().await?;
    checkout.cleanup().await?;
    assert_eq!(fs::read_dir(storage.path())?.count(), 0);
    Ok(())
}
