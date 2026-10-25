//! Bounded workspace archive loading and atomic saving.
//!
//! The archive path is trusted caller input. Both operations run on a blocking
//! worker; callers supply the per-file cap and snapshot budget.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use circus_sandbox::{CodeSnapshot, SnapshotBudget, SnapshotError};

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("workspace archive is not a regular file")]
    NotRegular,
    #[error("workspace archive is invalid or exceeds its budgets")]
    Invalid,
    #[error("workspace archive could not be read")]
    Read,
    #[error("workspace archive could not be written; the previous archive is unchanged")]
    Write,
}

/// Loads an existing archive before any sandbox is created. A missing path is a
/// fresh workspace; any other failure is fatal and leaves the old archive.
pub async fn load(
    path: &Path,
    max_file_bytes: usize,
    budget: SnapshotBudget,
) -> Result<Option<Arc<CodeSnapshot>>, ArchiveError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || load_blocking(&path, max_file_bytes, budget))
        .await
        .map_err(|_| ArchiveError::Read)?
}

fn load_blocking(
    path: &Path,
    max_file_bytes: usize,
    budget: SnapshotBudget,
) -> Result<Option<Arc<CodeSnapshot>>, ArchiveError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ArchiveError::Read),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ArchiveError::NotRegular);
    }
    CodeSnapshot::load_file(path, max_file_bytes, budget)
        .map(|snapshot| Some(Arc::new(snapshot)))
        .map_err(|error| match error {
            SnapshotError::Io { .. } => ArchiveError::Read,
            _ => ArchiveError::Invalid,
        })
}

/// Publishes a captured workspace with the snapshot API's same-directory
/// temporary file and rename. A failed save never truncates the old archive.
pub async fn save(snapshot: Arc<CodeSnapshot>, path: PathBuf) -> Result<(), ArchiveError> {
    tokio::task::spawn_blocking(move || snapshot.save_file(&path))
        .await
        .map_err(|_| ArchiveError::Write)?
        .map_err(|_| ArchiveError::Write)
}

#[cfg(test)]
mod tests {
    use super::*;
    use circus_sandbox::Workspace;

    fn budget() -> SnapshotBudget {
        SnapshotBudget::new(1024, 32).unwrap()
    }

    #[tokio::test]
    async fn missing_archive_is_a_fresh_workspace() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load(&dir.path().join("missing.snap"), 1024, budget())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn malformed_archive_is_rejected_before_use() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.snap");
        std::fs::write(&path, b"garbage").unwrap();
        assert!(matches!(
            load(&path, 1024, budget()).await,
            Err(ArchiveError::Invalid)
        ));
    }

    #[tokio::test]
    async fn saved_snapshot_round_trips_through_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.snap");
        let workspace = Workspace::new(1024).unwrap();
        workspace.write_file("note.txt", b"hello").unwrap();
        let snapshot = workspace.capture_code_snapshot(budget()).unwrap();
        save(Arc::new(snapshot), path.clone()).await.unwrap();
        let loaded = load(&path, 1024, budget()).await.unwrap().unwrap();
        let restored = loaded.restore().unwrap();
        assert_eq!(restored.read_file("note.txt").unwrap(), b"hello");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_archive_is_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.snap");
        let link = dir.path().join("link.snap");
        std::fs::write(&target, b"data").unwrap();
        symlink(&target, &link).unwrap();
        assert!(matches!(
            load(&link, 1024, budget()).await,
            Err(ArchiveError::NotRegular)
        ));
    }
}
