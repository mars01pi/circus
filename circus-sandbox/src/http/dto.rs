//! JSON request and response models for the HTTP transport.
//!
//! These are transport DTOs only. They are converted to the crate's neutral
//! configuration and request values before any domain call, and every failing
//! conversion becomes `invalid_request` or `unsupported_backend` without
//! echoing the supplied value.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use crate::api::{
    BackendConfig, CommandOutput, CommandRequest, EnforcementReport, SandboxConfig, SandboxPhase,
    SnapshotBudget,
};
use crate::runtime::{IoLimits, IsolationConfig, ResourceLimits};

use super::error::ApiError;

const MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateSandboxRequest {
    pub(super) backend: BackendDto,
    pub(super) sandbox: SandboxDto,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BackendDto {
    pub(super) kind: String,
    pub(super) endpoint: String,
    pub(super) image: String,
    pub(super) runtime: String,
}

impl BackendDto {
    pub(super) fn into_config(self) -> Result<BackendConfig, ApiError> {
        if self.kind != "docker" {
            return Err(ApiError::unsupported_backend());
        }
        if self.runtime != "runc" {
            return Err(ApiError::unsupported_backend());
        }
        if self.endpoint.is_empty() || self.image.is_empty() {
            return Err(ApiError::invalid_request());
        }
        Ok(BackendConfig::docker(
            self.endpoint,
            self.image,
            self.runtime,
        ))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SandboxDto {
    pub(super) memory_bytes: u64,
    pub(super) cpu_millis: u32,
    pub(super) pids: u32,
    pub(super) workspace_bytes: u64,
    pub(super) max_file_bytes: u64,
    pub(super) isolation: IsolationDto,
    pub(super) io: IoDto,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IsolationDto {
    pub(super) workspace_path: String,
    pub(super) guest_uid: u32,
    pub(super) guest_gid: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IoDto {
    pub(super) output_limit_bytes: u64,
    pub(super) command_timeout_ms: u64,
    pub(super) file_timeout_ms: u64,
    pub(super) lifecycle_timeout_ms: u64,
}

impl SandboxDto {
    pub(super) fn into_config(self) -> Result<SandboxConfig, ApiError> {
        let max_file_bytes =
            usize::try_from(self.max_file_bytes).map_err(|_| ApiError::invalid_request())?;
        let limits = ResourceLimits::new(
            self.memory_bytes,
            self.cpu_millis,
            self.pids,
            self.workspace_bytes,
            max_file_bytes,
        )
        .map_err(|_| ApiError::invalid_request())?;
        let IsolationDto {
            workspace_path,
            guest_uid,
            guest_gid,
        } = self.isolation;
        let isolation = IsolationConfig::linux_enforced(
            true,
            true,
            true,
            true,
            workspace_path,
            guest_uid,
            guest_gid,
        )
        .map_err(|_| ApiError::invalid_request())?;
        let output_limit_bytes =
            usize::try_from(self.io.output_limit_bytes).map_err(|_| ApiError::invalid_request())?;
        let io = IoLimits::with_deadlines(
            output_limit_bytes,
            timeout(self.io.command_timeout_ms)?,
            timeout(self.io.file_timeout_ms)?,
            timeout(self.io.lifecycle_timeout_ms)?,
        )
        .map_err(|_| ApiError::invalid_request())?;
        SandboxConfig::new(limits, isolation, io).map_err(|_| ApiError::invalid_request())
    }
}

fn timeout(millis: u64) -> Result<Duration, ApiError> {
    if millis == 0 || millis > MAX_TIMEOUT_MS {
        return Err(ApiError::invalid_request());
    }
    Ok(Duration::from_millis(millis))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecDto {
    pub(super) argv: Vec<String>,
    pub(super) timeout_ms: u64,
    pub(super) output_limit_bytes: u64,
}

impl ExecDto {
    pub(super) fn into_request(self) -> Result<CommandRequest, ApiError> {
        let output_limit_bytes =
            usize::try_from(self.output_limit_bytes).map_err(|_| ApiError::invalid_request())?;
        if output_limit_bytes == 0 {
            return Err(ApiError::invalid_request());
        }
        Ok(CommandRequest {
            argv: self.argv,
            timeout: timeout(self.timeout_ms)?,
            output_limit_bytes,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CaptureDto {
    pub(super) max_total_bytes: u64,
    pub(super) max_entries: u64,
    pub(super) max_archive_bytes: u64,
}

impl CaptureDto {
    pub(super) fn budget(&self) -> Result<(SnapshotBudget, u64), ApiError> {
        let max_total_bytes =
            usize::try_from(self.max_total_bytes).map_err(|_| ApiError::invalid_request())?;
        let max_entries =
            usize::try_from(self.max_entries).map_err(|_| ApiError::invalid_request())?;
        let budget = SnapshotBudget::new(max_total_bytes, max_entries)
            .map_err(|_| ApiError::invalid_request())?;
        Ok((budget, self.max_archive_bytes))
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct IdPath {
    pub(super) id: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct FilePath {
    pub(super) id: String,
    pub(super) path: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct SnapshotPath {
    pub(super) id: String,
    pub(super) snapshot_id: String,
}

#[derive(Debug, Serialize)]
pub(super) struct SandboxBody {
    pub(super) id: String,
    pub(super) phase: &'static str,
    pub(super) enforcement: Option<EnforcementBody>,
}

impl SandboxBody {
    pub(super) fn new(
        id: String,
        phase: SandboxPhase,
        enforcement: Option<EnforcementReport>,
    ) -> Self {
        Self {
            id,
            phase: phase_name(phase),
            enforcement: enforcement.map(EnforcementBody::from),
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct EnforcementBody {
    pub(super) runtime: Option<String>,
    pub(super) memory_bytes: Option<i64>,
    pub(super) nano_cpus: Option<i64>,
    pub(super) pids_limit: Option<i64>,
    pub(super) readonly_root: Option<bool>,
    pub(super) network_mode: Option<String>,
    pub(super) cap_drop: Option<Vec<String>>,
    pub(super) security_opt: Option<Vec<String>>,
    pub(super) bind_count: Option<usize>,
    pub(super) tmpfs_workspace: Option<String>,
}

impl From<EnforcementReport> for EnforcementBody {
    fn from(report: EnforcementReport) -> Self {
        Self {
            runtime: report.runtime,
            memory_bytes: report.memory_bytes,
            nano_cpus: report.nano_cpus,
            pids_limit: report.pids_limit,
            readonly_root: report.readonly_root,
            network_mode: report.network_mode,
            cap_drop: report.cap_drop,
            security_opt: report.security_opt,
            bind_count: report.bind_count,
            tmpfs_workspace: report.tmpfs_workspace,
        }
    }
}

pub(super) fn phase_name(phase: SandboxPhase) -> &'static str {
    match phase {
        SandboxPhase::Created => "created",
        SandboxPhase::Running => "running",
        SandboxPhase::Paused => "paused",
        SandboxPhase::Destroyed => "destroyed",
        SandboxPhase::CleanupRequired => "cleanup_required",
    }
}

#[derive(Debug, Serialize)]
pub(super) struct ExecBody {
    pub(super) exit_code: i64,
    pub(super) stdout_base64: String,
    pub(super) stderr_base64: String,
}

impl From<CommandOutput> for ExecBody {
    fn from(output: CommandOutput) -> Self {
        Self {
            exit_code: output.exit_code,
            stdout_base64: BASE64.encode(output.stdout),
            stderr_base64: BASE64.encode(output.stderr),
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct SnapshotBody {
    pub(super) snapshot_id: String,
    pub(super) entries: usize,
    pub(super) total_bytes: usize,
}

#[derive(Debug, Serialize)]
pub(super) struct HealthBody {
    pub(super) status: &'static str,
}
