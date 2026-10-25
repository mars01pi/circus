use super::*;

#[tokio::test]
async fn abandoned_startup_drains_allocation_and_explicitly_removes_it() {
    let storage = tempfile::tempdir().unwrap();
    let source = GitSource::local(storage.path().to_owned()).unwrap();
    let revision = GitRevision::parse(&"0".repeat(40)).unwrap();
    let executable = if cfg!(windows) {
        "C:/Program Files/Git/cmd/git.exe"
    } else {
        "/usr/bin/git"
    };
    let options = GitOptions::new(
        executable.into(),
        storage.path().to_owned(),
        Duration::from_secs(30),
        1024,
    )
    .unwrap();
    let cancellation = CancellationToken::new();
    let (sender, requests) = mpsc::unbounded_channel();
    let (ready, startup) = oneshot::channel();
    // Equivalent to dropping clone after it enqueues its owner: the startup
    // receiver disappears and its drop guard cancels the shared token.
    drop(startup);
    cancellation.cancel();
    drop(sender);
    let owner = tokio::spawn(owner::serve(
        source,
        revision,
        options,
        cancellation,
        requests,
        ready,
    ));
    tokio::time::timeout(Duration::from_secs(10), owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read_dir(storage.path()).unwrap().count(), 0);
}
