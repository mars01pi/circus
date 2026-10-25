//! Durable, atomic recovery state.
//!
//! The state file is the only cross-process memory the proxy has. It is written
//! before a backend instance may exist, so a lost handle never leaves the next
//! run unable to name the possible orphan. A phase is deliberately
//! conservative: only confirmed backend absence plus completed staging cleanup
//! allows `Closed`.

use std::io::{Read, Write};
use std::path::Path;

use circus_sandbox::RecoveryId;
use serde::{Deserialize, Serialize};

const MAX_STATE_BYTES: u64 = 64 * 1024;
const TEMP_PREFIX: &str = ".circus-sandbox-exec-state-";
const KINDS: [&str; 3] = ["docker", "containerd", "kubernetes"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatePhase {
    /// Written before creation. An instance may exist or may materialize late.
    Creating,
    /// The sandbox is running and the proxy owns the only live handle.
    Running,
    /// Cleanup was confirmed absent and staging completion was reported.
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub recovery_id: RecoveryId,
    pub backend_kind: &'static str,
    pub phase: StatePhase,
}

impl State {
    #[must_use]
    pub fn new(recovery_id: RecoveryId, backend_kind: &'static str, phase: StatePhase) -> Self {
        Self {
            recovery_id,
            backend_kind,
            phase,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("state path is not a regular file")]
    NotRegular,
    #[error("state file exceeds its size limit")]
    TooLarge,
    #[error("state file could not be read")]
    Read,
    #[error("state file is not valid JSON")]
    Json,
    #[error("state file version must be 1")]
    Version,
    #[error("state file contains an invalid recovery id")]
    InvalidRecoveryId,
    #[error("state file contains an unknown backend kind")]
    BackendKind,
    #[error("state file belongs to backend {stored}, but the configuration selects {configured}")]
    BackendMismatch {
        stored: &'static str,
        configured: &'static str,
    },
    #[error("state file could not be written ({operation})")]
    Write { operation: &'static str },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredState {
    version: u8,
    recovery_id: String,
    backend_kind: String,
    phase: StatePhase,
}

/// Reads state atomically from the caller-owned path. A missing file is not an
/// error; a malformed file is a fail-closed error.
pub fn read(path: &Path) -> Result<Option<State>, StateError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(StateError::Read),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StateError::NotRegular);
    }
    if metadata.len() > MAX_STATE_BYTES {
        return Err(StateError::TooLarge);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_STATE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|_| StateError::Read)?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(StateError::TooLarge);
    }
    let stored: StoredState = sonic_rs::from_slice(&bytes).map_err(|_| StateError::Json)?;
    if stored.version != 1 {
        return Err(StateError::Version);
    }
    let recovery_id =
        RecoveryId::parse(&stored.recovery_id).map_err(|_| StateError::InvalidRecoveryId)?;
    let backend_kind = KINDS
        .into_iter()
        .find(|kind| *kind == stored.backend_kind.as_str())
        .ok_or(StateError::BackendKind)?;
    Ok(Some(State {
        recovery_id,
        backend_kind,
        phase: stored.phase,
    }))
}

/// Refuses to reuse a state file created for another backend. Otherwise an
/// orphan from the previous backend could be missed and duplicated.
pub fn validate_backend(state: &State, configured: &'static str) -> Result<(), StateError> {
    if state.backend_kind == configured {
        Ok(())
    } else {
        Err(StateError::BackendMismatch {
            stored: state.backend_kind,
            configured,
        })
    }
}

/// Atomically replaces the state file with a same-directory temporary file.
/// On Unix the temporary file is created with mode 0600 before its contents are
/// written. Parent directories must already exist.
pub fn write(path: &Path, state: &State) -> Result<(), StateError> {
    let stored = StoredState {
        version: 1,
        recovery_id: state.recovery_id.as_str().to_string(),
        backend_kind: state.backend_kind.to_string(),
        phase: state.phase,
    };
    let bytes = sonic_rs::to_vec(&stored).map_err(|_| StateError::Write {
        operation: "encode",
    })?;
    let parent = path.parent().ok_or(StateError::Write {
        operation: "parent directory",
    })?;
    let mut temporary = tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .tempfile_in(parent)
        .map_err(|_| StateError::Write {
            operation: "create temporary file",
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|_| StateError::Write {
                operation: "set permissions",
            })?;
    }
    temporary
        .as_file_mut()
        .write_all(&bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|_| StateError::Write { operation: "write" })?;
    temporary.persist(path).map_err(|_| StateError::Write {
        operation: "rename",
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::cleanup_confirmed;
    use circus_sandbox::{CleanupReport, Ownership};

    #[test]
    fn round_trips_every_phase_and_stays_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        assert_eq!(read(&path).unwrap(), None);
        let recovery = RecoveryId::generate();
        for phase in [
            StatePhase::Creating,
            StatePhase::Running,
            StatePhase::Closed,
        ] {
            let state = State::new(recovery.clone(), "docker", phase);
            write(&path, &state).unwrap();
            assert_eq!(read(&path).unwrap(), Some(state));
        }
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "temporary state file was retained");
    }

    #[cfg(unix)]
    #[test]
    fn state_file_is_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        write(
            &path,
            &State::new(RecoveryId::generate(), "containerd", StatePhase::Creating),
        )
        .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn malformed_or_foreign_state_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(matches!(read(&path), Err(StateError::Json)));

        let wrong_version = format!(
            r#"{{"version":2,"recoveryId":"{}","backendKind":"docker","phase":"running"}}"#,
            RecoveryId::generate()
        );
        std::fs::write(&path, wrong_version).unwrap();
        assert!(matches!(read(&path), Err(StateError::Version)));

        let uppercase = format!(
            r#"{{"version":1,"recoveryId":"{}","backendKind":"docker","phase":"running"}}"#,
            RecoveryId::generate().as_str().to_uppercase()
        );
        std::fs::write(&path, uppercase).unwrap();
        assert!(matches!(read(&path), Err(StateError::InvalidRecoveryId)));

        let foreign = format!(
            r#"{{"version":1,"recoveryId":"{}","backendKind":"nope","phase":"running"}}"#,
            RecoveryId::generate()
        );
        std::fs::write(&path, foreign).unwrap();
        assert!(matches!(read(&path), Err(StateError::BackendKind)));

        let state = State::new(RecoveryId::generate(), "docker", StatePhase::Running);
        write(&path, &state).unwrap();
        assert!(matches!(
            validate_backend(&state, "containerd"),
            Err(StateError::BackendMismatch { .. })
        ));
        assert!(validate_backend(&state, "docker").is_ok());
    }

    #[test]
    fn only_confirmed_absence_with_finished_staging_closes_state() {
        let full = CleanupReport {
            instance_id: None,
            ownership: Ownership::ConfirmedAbsent,
            detail: String::new(),
        };
        let staging = CleanupReport {
            instance_id: Some("circus-sandbox-id".to_string()),
            ownership: Ownership::ConfirmedAbsent,
            detail: String::new(),
        };
        let unknown = CleanupReport {
            instance_id: Some("circus-sandbox-id".to_string()),
            ownership: Ownership::Unknown,
            detail: String::new(),
        };
        assert!(cleanup_confirmed(&full));
        assert!(!cleanup_confirmed(&staging));
        assert!(!cleanup_confirmed(&unknown));
    }
}
