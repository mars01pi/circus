//! Bounded staging transfer owned by the lifecycle worker.
//!
//! The worker walks staging once, validates every relative path and metadata
//! bound, and only then starts the instance. Regular files are opened one at a
//! time and handed to the backend as an owned host handle with an explicit
//! length, so the transfer never builds a `Vec` over all staged payloads. The
//! backend copies each handle in bounded chunks.

use std::io;
use std::sync::Arc;

use super::operations::spawn_block;
use super::{SandboxError, State};
use crate::infrastructure::{Backend, EngineError, InstanceId, UploadEntry, UploadSource};
use crate::runtime::check_relative;
use crate::workspace::{StagedNode, WorkspaceError};

/// One validated staging entry. `len` is metadata observed during the plan
/// walk; it is never a copy of the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StagedEntry {
    pub relative: String,
    pub is_dir: bool,
    pub len: u64,
}

/// Validated staging metadata for one `start` attempt.
#[derive(Debug)]
pub(super) struct Plan {
    entries: Vec<StagedEntry>,
    total_bytes: u64,
}

impl Plan {
    pub(super) fn entries(&self) -> &[StagedEntry] {
        &self.entries
    }

    #[cfg(test)]
    pub(super) fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
}

/// Why one entry could not be transferred. Engine failures keep their typed
/// cleanup path; workspace failures were discovered after the instance started
/// and are reported without being rewritten as backend text.
#[derive(Debug)]
pub(super) enum TransferFailure {
    Engine(EngineError),
    Workspace(WorkspaceError),
    Worker(SandboxError),
}

/// Walks staging and enforces path and byte bounds without reading payloads.
pub(super) async fn plan<B: Backend>(state: &State<B>) -> Result<Plan, SandboxError> {
    let workspace = Arc::clone(&state.workspace);
    let nodes = spawn_block(move || {
        workspace
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .staged_nodes()
    })
    .await??;
    let plan = build_plan(
        nodes,
        state.config.limits().max_file_bytes(),
        state.config.limits().workspace_bytes(),
    )?;
    // Open each file once before the instance starts. This keeps ordinary
    // readability and size disagreements in `Created`, preserving the existing
    // staging failure semantics. Payload bytes are still not read here.
    verify_readable(state, &plan).await?;
    Ok(plan)
}

async fn verify_readable<B: Backend>(state: &State<B>, plan: &Plan) -> Result<(), SandboxError> {
    for entry in plan.entries().iter().filter(|entry| !entry.is_dir) {
        let planned_len = entry.len;
        let workspace = Arc::clone(&state.workspace);
        let relative = entry.relative.clone();
        let (opened_len, _file) = spawn_block(move || {
            workspace
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .open_staged_file(&relative)
        })
        .await??;
        if opened_len != planned_len {
            return Err(WorkspaceError::Io(io::Error::other(format!(
                "staged file {} changed size during validation: planned {planned_len} bytes, opened {opened_len}",
                entry.relative
            )))
            .into());
        }
    }
    Ok(())
}

/// Validates one already-walked node list in walk order.
///
/// This is the pure planning half of `start`: path checks run before any byte
/// accounting, the per-file bound is checked from metadata, and the aggregate
/// uses the shared checked counter. It allocates one small entry per node, not
/// one payload per file.
pub(super) fn build_plan(
    nodes: Vec<StagedNode>,
    max_file_bytes: usize,
    workspace_bytes: u64,
) -> Result<Plan, SandboxError> {
    let mut entries = Vec::new();
    entries.try_reserve_exact(nodes.len()).map_err(|err| {
        SandboxError::Workspace(WorkspaceError::Io(io::Error::other(format!(
            "staging plan allocation failed: {err}"
        ))))
    })?;
    let mut total = 0u64;
    let per_file_limit = u64::try_from(max_file_bytes).unwrap_or(u64::MAX);
    for node in nodes {
        check_relative(&node.relative)?;
        if node.is_dir {
            entries.push(StagedEntry {
                relative: node.relative,
                is_dir: true,
                len: 0,
            });
            continue;
        }
        if node.len > per_file_limit {
            // A read probe would have observed at most `limit + 1` bytes;
            // preserve that same reported value.
            return Err(WorkspaceError::LimitExceeded {
                limit: max_file_bytes,
                actual: max_file_bytes.saturating_add(1),
            }
            .into());
        }
        total = super::input::staged_total(total, node.len, workspace_bytes)?;
        entries.push(StagedEntry {
            relative: node.relative,
            is_dir: false,
            len: node.len,
        });
    }
    Ok(Plan {
        entries,
        total_bytes: total,
    })
}

/// Transfers every planned entry sequentially. At most one host file handle
/// and one backend copy buffer are live at a time; the plan itself holds only
/// relative paths and lengths.
pub(super) async fn transfer<B: Backend>(
    state: &State<B>,
    id: &InstanceId,
    plan: &Plan,
) -> Result<(), TransferFailure> {
    let workspace_bytes = state.config.limits().workspace_bytes();
    let mut transferred = 0u64;
    for entry in plan.entries() {
        let upload = if entry.is_dir {
            UploadEntry {
                path: entry.relative.clone(),
                source: UploadSource::Directory,
            }
        } else {
            let planned_len = entry.len;
            transferred = transferred.saturating_add(planned_len);
            let workspace = Arc::clone(&state.workspace);
            let relative = entry.relative.clone();
            let (opened_len, file) = spawn_block(move || {
                workspace
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .open_staged_file(&relative)
            })
            .await
            .map_err(TransferFailure::Worker)?
            .map_err(TransferFailure::Workspace)?;
            if opened_len != planned_len {
                return Err(TransferFailure::Workspace(WorkspaceError::Io(
                    io::Error::other(format!(
                        "staged file {} changed size during start: planned {planned_len} bytes, opened {opened_len}",
                        entry.relative
                    )),
                )));
            }
            UploadEntry {
                path: entry.relative.clone(),
                source: UploadSource::File {
                    file,
                    len: opened_len,
                },
            }
        };
        state
            .backend
            .upload(
                id,
                &state.user,
                state.config.isolation().workspace_path(),
                upload,
                workspace_bytes,
            )
            .await
            .map_err(TransferFailure::Engine)?;
    }
    debug_assert_eq!(
        transferred, plan.total_bytes,
        "transfer accounting must match the validated plan"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::StagingError;
    use crate::runtime::ConfigError;

    fn node(relative: &str, is_dir: bool, len: u64) -> StagedNode {
        StagedNode {
            relative: relative.to_string(),
            is_dir,
            len,
        }
    }

    #[test]
    fn plan_preserves_walk_order_and_keeps_directories() {
        let nodes = vec![
            node("nested", true, 0),
            node("nested/inner", true, 0),
            node("nested/inner/file.txt", false, 5),
            node("root.txt", false, 3),
        ];
        let plan = build_plan(nodes, 8, 32).expect("plan fits");
        let entries = plan.entries();
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.relative.as_str(), entry.is_dir, entry.len))
                .collect::<Vec<_>>(),
            vec![
                ("nested", true, 0),
                ("nested/inner", true, 0),
                ("nested/inner/file.txt", false, 5),
                ("root.txt", false, 3),
            ]
        );
        assert_eq!(plan.total_bytes(), 8);
    }

    #[test]
    fn plan_rejects_aggregate_above_workspace_bound() {
        let nodes = vec![node("first", false, 700), node("second", false, 700)];
        let error = build_plan(nodes, 1024, 1024).expect_err("aggregate exceeds bound");
        assert!(matches!(
            error,
            SandboxError::Staging(StagingError::LimitExceeded {
                limit: 1024,
                actual: 1400,
            })
        ));
    }

    #[test]
    fn plan_rejects_payload_count_overflow() {
        let nodes = vec![
            node("huge", false, u64::MAX - 1),
            node("also-huge", false, u64::MAX - 1),
        ];
        let error = build_plan(nodes, usize::MAX - 1, u64::MAX).expect_err("sum overflows");
        assert!(matches!(
            error,
            SandboxError::Staging(StagingError::SizeOverflow {
                accumulated,
                next,
                limit: u64::MAX,
            }) if accumulated == u64::MAX - 1 && next == u64::MAX - 1
        ));
    }

    #[test]
    fn plan_rejects_file_above_per_file_bound_from_metadata() {
        let nodes = vec![node("big", false, 5)];
        let error = build_plan(nodes, 4, 1024).expect_err("file exceeds per-file bound");
        assert!(matches!(
            error,
            SandboxError::Workspace(WorkspaceError::LimitExceeded {
                limit: 4,
                actual: 5
            })
        ));
    }

    #[test]
    fn plan_rejects_non_portable_paths_before_payload_accounting() {
        let nodes = vec![node("../escape", false, 1)];
        let error = build_plan(nodes, 1024, 1024).expect_err("path must be rejected");
        assert!(matches!(
            error,
            SandboxError::Config(ConfigError::RelativePath { .. })
        ));
    }
}
