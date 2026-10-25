//! Pod exec transport: one-shot commands and the shared guest file helper.
//!
//! Kubernetes exec has no working-directory or user parameter, so the adapter
//! runs the configured command through a tiny argv-only Python trampoline that
//! changes directory and then `execvp`s the requested program. The guest file
//! protocol itself stays in `guest_transport`/`guest_files.py`. The bounded
//! attach buffer size and the status/stream error mapping are shared with the
//! interactive process transport in `kubernetes_process`.

use std::time::Duration;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
use kube::api::AttachParams;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::engine::{EngineError, EngineErrorKind, WorkspaceEntry};
use super::engine::{ExecDone, OutputCollector, OutputPhase, decode_manifest, engine_error_kind};
use super::guest_transport::{
    ExecIo, GuestFileMode, GuestStdin, TRANSFER_CHUNK_BYTES, helper_argv, helper_failure,
    helper_limits,
};
use super::kubernetes_engine::{KubernetesEngine, engine_error};

/// Bounded attach buffers shared with the interactive process transport.
pub(super) const ATTACH_BUFFER_BYTES: usize = 64 * 1024;
const CHDIR_EXEC: &str = "import os,sys\ntry:\n    os.chdir(sys.argv[1])\nexcept OSError as error:\n    sys.stderr.write(f\"circus: cannot change directory: {error}\\n\")\n    raise SystemExit(125)\nos.execvp(sys.argv[2], sys.argv[2:])";

struct FileRequest<'a> {
    relative: &'a str,
    mode: GuestFileMode,
    stdin: GuestStdin<'a>,
    max_bytes: usize,
    max_entries: Option<usize>,
}

impl KubernetesEngine {
    /// Runs one argv command and returns its exit status.
    ///
    /// `user` is accepted for backend neutrality but cannot be set by the
    /// Kubernetes exec API; the Pod's container security context already fixes
    /// the guest uid/gid.
    pub(super) async fn exec(
        &self,
        name: &str,
        _user: &str,
        workdir: &str,
        argv: &[String],
        output_limit: usize,
    ) -> Result<ExecDone, EngineError> {
        let command = with_workdir(workdir, argv);
        let began = std::time::Instant::now();
        let result = self
            .exec_io(
                name,
                &command,
                ExecIo {
                    stdin: GuestStdin::None,
                    stdout_limit: output_limit,
                    stderr_limit: output_limit,
                    combined_limit: output_limit,
                },
            )
            .await;
        let duration_ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
        match &result {
            Ok(done) => crate::observability::events::backend_exec_completed(
                duration_ms,
                Some(done.exit_code),
                done.stdout.len(),
                done.stderr.len(),
                "none",
                "ok",
            ),
            Err(err) => crate::observability::events::backend_exec_completed(
                duration_ms,
                None,
                0,
                0,
                "none",
                engine_error_kind(err),
            ),
        }
        result
    }

    /// Streams one staging entry through the confined guest helper.
    pub(super) async fn upload(
        &self,
        name: &str,
        user: &str,
        workspace: &str,
        entry: super::engine::UploadEntry,
        max_bytes: u64,
    ) -> Result<(), EngineError> {
        let super::engine::UploadEntry { path, source } = entry;
        match source {
            super::engine::UploadSource::Directory => {
                self.file_helper(
                    name,
                    user,
                    workspace,
                    FileRequest {
                        relative: &path,
                        mode: GuestFileMode::Mkdir,
                        stdin: GuestStdin::None,
                        max_bytes: 0,
                        max_entries: None,
                    },
                )
                .await?;
            }
            super::engine::UploadSource::Bytes(bytes) => {
                if bytes.len() as u64 > max_bytes {
                    return Err(limit_error("staged bytes exceed workspace limit"));
                }
                self.file_helper(
                    name,
                    user,
                    workspace,
                    FileRequest {
                        relative: &path,
                        mode: GuestFileMode::Write,
                        stdin: GuestStdin::Bytes(&bytes),
                        max_bytes: 0,
                        max_entries: None,
                    },
                )
                .await?;
            }
            super::engine::UploadSource::File { file, len } => {
                if len > max_bytes {
                    return Err(limit_error("staged bytes exceed workspace limit"));
                }
                self.file_helper(
                    name,
                    user,
                    workspace,
                    FileRequest {
                        relative: &path,
                        mode: GuestFileMode::Write,
                        stdin: GuestStdin::File { file, len },
                        max_bytes: 0,
                        max_entries: None,
                    },
                )
                .await?;
            }
        }
        Ok(())
    }

    pub(super) async fn download_file(
        &self,
        name: &str,
        user: &str,
        workspace: &str,
        relative: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, EngineError> {
        let bytes = self
            .file_helper(
                name,
                user,
                workspace,
                FileRequest {
                    relative,
                    mode: GuestFileMode::Read,
                    stdin: GuestStdin::None,
                    max_bytes,
                    max_entries: None,
                },
            )
            .await?;
        if bytes.len() > max_bytes {
            return Err(limit_error("file exceeds byte limit"));
        }
        Ok(bytes)
    }

    pub(super) async fn remove_file(
        &self,
        name: &str,
        user: &str,
        workspace: &str,
        relative: &str,
    ) -> Result<(), EngineError> {
        self.file_helper(
            name,
            user,
            workspace,
            FileRequest {
                relative,
                mode: GuestFileMode::Unlink,
                stdin: GuestStdin::None,
                max_bytes: 0,
                max_entries: None,
            },
        )
        .await?;
        Ok(())
    }

    /// Enumerates the configured workspace with the shared manifest protocol.
    pub(super) async fn list_workspace(
        &self,
        name: &str,
        user: &str,
        workspace: &str,
        max_bytes: u64,
        max_entries: usize,
    ) -> Result<Vec<WorkspaceEntry>, EngineError> {
        if max_bytes == 0 {
            return Err(limit_error(
                "workspace manifest byte limit must be positive",
            ));
        }
        let bytes = self
            .file_helper(
                name,
                user,
                workspace,
                FileRequest {
                    relative: "",
                    mode: GuestFileMode::List,
                    stdin: GuestStdin::None,
                    max_bytes: usize::try_from(max_bytes).unwrap_or(usize::MAX),
                    max_entries: Some(max_entries),
                },
            )
            .await?;
        decode_manifest(&bytes)
    }

    async fn file_helper(
        &self,
        name: &str,
        _user: &str,
        workspace: &str,
        request: FileRequest<'_>,
    ) -> Result<Vec<u8>, EngineError> {
        let FileRequest {
            relative,
            mode,
            stdin,
            max_bytes,
            max_entries,
        } = request;
        let bound = if mode == GuestFileMode::Write {
            stdin.bound()
        } else {
            u64::try_from(max_bytes).unwrap_or(u64::MAX)
        };
        let argv = helper_argv(workspace, relative, mode, bound, max_entries);
        let (stdout_limit, stderr_limit, combined_limit) = helper_limits(max_bytes);
        let result = self
            .exec_io(
                name,
                &argv,
                ExecIo {
                    stdin,
                    stdout_limit,
                    stderr_limit,
                    combined_limit,
                },
            )
            .await?;
        if result.exit_code == 0 {
            Ok(result.stdout)
        } else {
            Err(helper_failure(result.exit_code))
        }
    }

    async fn exec_io(
        &self,
        name: &str,
        argv: &[String],
        io: ExecIo<'_>,
    ) -> Result<ExecDone, EngineError> {
        if argv.is_empty() {
            return Err(engine_error(
                EngineErrorKind::InvalidPath,
                "exec needs a nonempty argv",
            ));
        }
        let ExecIo {
            stdin,
            stdout_limit,
            stderr_limit,
            combined_limit,
        } = io;
        let params = AttachParams {
            stdin: stdin.attached(),
            stdout: true,
            stderr: true,
            tty: false,
            container: None,
            max_stdin_buf_size: Some(ATTACH_BUFFER_BYTES),
            max_stdout_buf_size: Some(ATTACH_BUFFER_BYTES),
            max_stderr_buf_size: Some(ATTACH_BUFFER_BYTES),
        };
        let mut process = self
            .pods
            .exec(name, argv.to_vec(), &params)
            .await
            .map_err(|err| {
                engine_error(
                    EngineErrorKind::Protocol,
                    format!("pods/exec attach failed: {err}"),
                )
            })?;
        let stdin_writer = process.stdin();
        let stdout = process
            .stdout()
            .ok_or_else(|| engine_error(EngineErrorKind::Protocol, "exec did not attach stdout"))?;
        let stderr = process
            .stderr()
            .ok_or_else(|| engine_error(EngineErrorKind::Protocol, "exec did not attach stderr"))?;
        let status = process.take_status().ok_or_else(|| {
            engine_error(
                EngineErrorKind::Protocol,
                "exec did not expose a status channel",
            )
        })?;
        let send = send_stdin(stdin_writer, stdin);
        let receive = collect(stdout, stderr, stdout_limit, stderr_limit, combined_limit);
        let (sent, received) = tokio::join!(send, receive);
        let (stdout, stderr) = received?;
        process.join().await.map_err(|err| {
            engine_error(
                EngineErrorKind::Protocol,
                format!("pods/exec stream failed: {err}"),
            )
        })?;
        let exit_code = status.await.map_or(-1, |status| exit_code(Some(&status)));
        if exit_code == 0 {
            sent?;
        }
        Ok(ExecDone {
            exit_code,
            stdout,
            stderr,
        })
    }
}

/// Prefixes an argv-only directory change. `/` needs no change and keeps the
/// exact user command.
fn with_workdir(workdir: &str, argv: &[String]) -> Vec<String> {
    if workdir == "/" || workdir.is_empty() {
        return argv.to_vec();
    }
    let mut command = vec![
        "python3".to_string(),
        "-I".to_string(),
        "-B".to_string(),
        "-c".to_string(),
        CHDIR_EXEC.to_string(),
        workdir.to_string(),
    ];
    command.extend_from_slice(argv);
    command
}

async fn send_stdin<W: AsyncWrite + Unpin>(
    writer: Option<W>,
    stdin: GuestStdin<'_>,
) -> Result<(), EngineError> {
    let Some(mut writer) = writer else {
        return Ok(());
    };
    match stdin {
        GuestStdin::None => {}
        GuestStdin::Bytes(bytes) => {
            writer
                .write_all(bytes)
                .await
                .map_err(|err| engine_error(EngineErrorKind::Protocol, err.to_string()))?;
        }
        GuestStdin::File { file, len } => {
            let mut source = tokio::fs::File::from_std(file);
            let mut chunk = vec![0u8; TRANSFER_CHUNK_BYTES];
            let mut remaining = len;
            while remaining > 0 {
                let want = usize::try_from(remaining)
                    .unwrap_or(TRANSFER_CHUNK_BYTES)
                    .min(chunk.len());
                let read = source.read(&mut chunk[..want]).await.map_err(|err| {
                    engine_error(EngineErrorKind::FileIo, format!("staged file read: {err}"))
                })?;
                if read == 0 {
                    return Err(engine_error(
                        EngineErrorKind::FileIo,
                        format!("staged file ended before its declared {len} byte bound"),
                    ));
                }
                writer
                    .write_all(&chunk[..read])
                    .await
                    .map_err(|err| engine_error(EngineErrorKind::Protocol, err.to_string()))?;
                remaining -= u64::try_from(read).unwrap_or(0);
            }
        }
    }
    writer
        .shutdown()
        .await
        .map_err(|err| engine_error(EngineErrorKind::Protocol, err.to_string()))
}

async fn collect<O: AsyncRead + Unpin, E: AsyncRead + Unpin>(
    mut stdout: O,
    mut stderr: E,
    stdout_limit: usize,
    stderr_limit: usize,
    combined_limit: usize,
) -> Result<(Vec<u8>, Vec<u8>), EngineError> {
    let mut collector = OutputCollector::new(stdout_limit, stderr_limit, combined_limit);
    let mut stdout_buf = vec![0u8; TRANSFER_CHUNK_BYTES];
    let mut stderr_buf = vec![0u8; TRANSFER_CHUNK_BYTES];
    let mut stdout_done = false;
    let mut stderr_done = false;
    while !(stdout_done && stderr_done) {
        tokio::select! {
            result = stdout.read(&mut stdout_buf), if !stdout_done => {
                let read = result.map_err(stream_error)?;
                if read == 0 {
                    stdout_done = true;
                } else {
                    collector.push(OutputPhase::Stdout, &stdout_buf[..read])?;
                }
            }
            result = stderr.read(&mut stderr_buf), if !stderr_done => {
                let read = result.map_err(stream_error)?;
                if read == 0 {
                    stderr_done = true;
                } else {
                    collector.push(OutputPhase::Stderr, &stderr_buf[..read])?;
                }
            }
        }
    }
    Ok(collector.finish())
}

pub(super) fn stream_error(err: std::io::Error) -> EngineError {
    engine_error(EngineErrorKind::Protocol, err.to_string())
}

fn limit_error(message: &'static str) -> EngineError {
    engine_error(EngineErrorKind::Limit, message)
}

/// Extracts the exit code from the exec status channel.
///
/// Kubernetes reports success directly and a non-zero status as
/// `NonZeroExitCode` with an `ExitCode` cause. An absent status is reported as
/// `-1` rather than assumed successful.
pub(super) fn exit_code(status: Option<&Status>) -> i64 {
    let Some(status) = status else {
        return -1;
    };
    if status.status.as_deref() == Some("Success") {
        return 0;
    }
    if let Some(cause) = status
        .details
        .as_ref()
        .and_then(|details| details.causes.as_ref())
        .and_then(|causes| {
            causes
                .iter()
                .find(|cause| cause.reason.as_deref() == Some("ExitCode"))
        })
        && let Some(code) = cause
            .message
            .as_deref()
            .and_then(|value| value.parse().ok())
    {
        return code;
    }
    let message = status.message.as_deref().unwrap_or_default();
    for token in message.split_whitespace() {
        if let Ok(code) = token.parse::<i64>() {
            return code;
        }
    }
    -1
}

// Keep an explicit duration import used by callers configuring timeouts.
const _: Duration = Duration::from_secs(1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workdir_is_argv_only() {
        let argv = vec!["echo".to_string(), "hello".to_string()];
        assert_eq!(with_workdir("/", &argv), argv);
        let prefixed = with_workdir("/workspace", &argv);
        assert_eq!(prefixed[0], "python3");
        assert_eq!(prefixed[5], "/workspace");
        assert_eq!(&prefixed[6..], &argv[..]);
    }

    #[test]
    fn exit_status_parsing_is_fail_closed() {
        assert_eq!(exit_code(None), -1);
        let success = Status {
            status: Some("Success".to_string()),
            ..Status::default()
        };
        assert_eq!(exit_code(Some(&success)), 0);
        let failure = Status {
            status: Some("Failure".to_string()),
            reason: Some("NonZeroExitCode".to_string()),
            details: Some(
                k8s_openapi::apimachinery::pkg::apis::meta::v1::StatusDetails {
                    causes: Some(vec![
                        k8s_openapi::apimachinery::pkg::apis::meta::v1::StatusCause {
                            reason: Some("ExitCode".to_string()),
                            message: Some("7".to_string()),
                            field: None,
                        },
                    ]),
                    ..Default::default()
                },
            ),
            ..Status::default()
        };
        assert_eq!(exit_code(Some(&failure)), 7);
    }
}
