//! Host-only credential sources. Errors deliberately carry no source content.

use std::io::Read;
use std::path::Path;

#[cfg(unix)]
use cap_fs_ext::OpenOptionsSyncExt;
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{File, OpenOptions};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeychainRef {
    pub service: String,
    pub account: String,
}

impl KeychainRef {
    fn validate(&self) -> Result<(), ()> {
        for value in [&self.service, &self.account] {
            if value.is_empty()
                || value.len() > 256
                || value.starts_with('-')
                || value.chars().any(char::is_control)
            {
                return Err(());
            }
        }
        Ok(())
    }
}

pub(crate) async fn read_secret(
    file: Option<&Path>,
    keychain: Option<&KeychainRef>,
    limit: usize,
) -> Result<Vec<u8>, ()> {
    match (file, keychain) {
        (Some(path), None) => read_bounded(path, limit).await,
        (None, Some(reference)) => {
            reference.validate()?;
            read_keychain(reference, limit).await
        }
        _ => Err(()),
    }
}

/// Only regular files, opened without following the final symlink. Nonblocking
/// Unix open prevents a FIFO from hanging configuration loading. This is a host
/// operator path, not a sandbox capability or protection against host root.
pub(crate) async fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, ()> {
    if !path.is_absolute() || limit == 0 || limit > 1024 * 1024 {
        return Err(());
    }
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        options.nonblock(true);
        let file = File::open_ambient_with(&path, &options, cap_std::ambient_authority())
            .map_err(|_| ())?;
        let metadata = file.metadata().map_err(|_| ())?;
        if !metadata.is_file() || metadata.len() > limit as u64 {
            return Err(());
        }
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ())?;
        if bytes.len() > limit {
            return Err(());
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| ())?
}

#[cfg(target_os = "macos")]
async fn read_keychain(reference: &KeychainRef, limit: usize) -> Result<Vec<u8>, ()> {
    let mut command = tokio::process::Command::new("/usr/bin/security");
    command.args([
        "find-generic-password",
        "-s",
        &reference.service,
        "-a",
        &reference.account,
        "-w",
    ]);
    capture_secret(&mut command, limit, std::time::Duration::from_secs(10)).await
}

#[cfg(not(target_os = "macos"))]
async fn read_keychain(_reference: &KeychainRef, _limit: usize) -> Result<Vec<u8>, ()> {
    // Never silently fall back to a file or environment variable.
    Err(())
}

#[cfg(any(target_os = "macos", all(test, unix)))]
async fn capture_secret(
    command: &mut tokio::process::Command,
    limit: usize,
    deadline: std::time::Duration,
) -> Result<Vec<u8>, ()> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;

    if limit == 0 || limit > 1024 * 1024 {
        return Err(());
    }
    // Passwords are returned only over this private pipe. stderr is never
    // surfaced: platform diagnostics can include credential metadata.
    let mut child = command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ())?;
    let result = tokio::time::timeout(deadline, async {
        let stdout = child.stdout.take().ok_or(())?;
        let mut bytes = Vec::new();
        stdout
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| ())?;
        if bytes.len() > limit {
            return Err(());
        }
        let status = child.wait().await.map_err(|_| ())?;
        if !status.success() {
            return Err(());
        }
        Ok(bytes)
    })
    .await
    .unwrap_or(Err(()));
    if result.is_err() {
        // Reap on timeout, excess output, and I/O failure before returning.
        let _ = child.start_kill();
        child.wait().await.map_err(|_| ())?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn requires_exactly_one_source() {
        let reference = KeychainRef {
            service: "circus-test".into(),
            account: "test".into(),
        };
        assert!(read_secret(None, None, 16).await.is_err());
        assert!(
            read_secret(Some(Path::new("/unused")), Some(&reference), 16)
                .await
                .is_err()
        );
        for service in ["", "-invalid", "invalid\nname"] {
            let reference = KeychainRef {
                service: service.into(),
                account: "test".into(),
            };
            assert!(read_secret(None, Some(&reference), 16).await.is_err());
        }
        assert!(
            sonic_rs::from_str::<KeychainRef>(
                r#"{"service":"test","account":"test","password":"rejected"}"#
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn reads_exact_limit_but_rejects_nonfiles_and_oversize() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("key");
        std::fs::write(&path, b"test-secret").unwrap();
        assert_eq!(
            read_secret(Some(&path), None, 11).await.unwrap(),
            b"test-secret"
        );
        assert!(read_secret(Some(&path), None, 10).await.is_err());
        assert!(read_bounded(directory.path(), 100).await.is_err());
        assert!(read_bounded(Path::new("relative"), 100).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinks_and_fifo_without_waiting_for_a_writer() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("secret");
        let link = directory.path().join("link");
        let fifo = directory.path().join("fifo");
        std::fs::write(&file, b"test-secret").unwrap();
        std::os::unix::fs::symlink(file, &link).unwrap();
        assert!(read_bounded(&link, 100).await.is_err());
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), read_bounded(&fifo, 100)).await;
        assert!(result.unwrap().is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_output_is_bounded_and_failures_have_no_details() {
        use std::time::Duration;
        use tokio::process::Command;
        let mut good = Command::new("/bin/sh");
        good.args(["-c", "printf test-secret"]);
        assert_eq!(
            capture_secret(&mut good, 11, Duration::from_secs(2))
                .await
                .unwrap(),
            b"test-secret"
        );
        for script in [
            "printf test-secret",
            "printf test-secret >&2; exit 1",
            "exec sleep 5",
        ] {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", script]);
            assert_eq!(
                capture_secret(&mut command, 10, Duration::from_millis(100)).await,
                Err(())
            );
        }
    }
}
