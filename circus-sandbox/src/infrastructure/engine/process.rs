//! Attached Docker exec transport. Lifecycle owns cancellation and cleanup.

use std::pin::Pin;

use futures_util::{Stream, StreamExt};
use tokio::io::AsyncWrite;

use super::{Engine, EngineError, map_err};

/// One attached process request. The backend supplies the instance identity.
pub(crate) struct ProcessSpec<'a> {
    pub user: &'a str,
    pub workdir: &'a str,
    pub argv: &'a [String],
    pub env: Vec<String>,
}

/// Raw, ordered transport chunk. Not a line and may split UTF-8 characters.
pub(crate) enum ProcessChunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

/// Engine-level attach handle. The exec identity is engine-owned and never
/// crosses the backend boundary.
pub(crate) struct EngineProcess {
    pub id: String,
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
    pub output: Pin<Box<dyn Stream<Item = Result<ProcessChunk, EngineError>> + Send>>,
}

impl Engine {
    pub(crate) async fn spawn_process(
        &self,
        container: &str,
        spec: ProcessSpec<'_>,
    ) -> Result<EngineProcess, EngineError> {
        let created = self
            .docker
            .create_exec(
                container,
                bollard::models::ExecConfig {
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    cmd: Some(spec.argv.to_vec()),
                    env: Some(spec.env),
                    user: Some(spec.user.to_string()),
                    working_dir: Some(spec.workdir.to_string()),
                    tty: Some(false),
                    privileged: Some(false),
                    ..Default::default()
                },
            )
            .await
            .map_err(map_err)?;
        let started = self
            .docker
            .start_exec(&created.id, None)
            .await
            .map_err(map_err)?;
        let bollard::exec::StartExecResults::Attached { output, input } = started else {
            return Err(EngineError::new("interactive exec did not attach"));
        };
        let output = output.map(|item| {
            item.map_err(map_err).map(|item| match item {
                bollard::container::LogOutput::StdErr { message } => {
                    ProcessChunk::Stderr(message.to_vec())
                }
                bollard::container::LogOutput::StdOut { message }
                | bollard::container::LogOutput::Console { message }
                | bollard::container::LogOutput::StdIn { message } => {
                    ProcessChunk::Stdout(message.to_vec())
                }
            })
        });
        Ok(EngineProcess {
            id: created.id,
            input: Box::pin(input),
            output: Box::pin(output),
        })
    }

    /// Confirms the exit status after the attached output stream has ended.
    pub(crate) async fn process_exit(&self, id: &str) -> Result<i64, EngineError> {
        let status = self.docker.inspect_exec(id).await.map_err(map_err)?;
        if status.running != Some(false) {
            return Err(EngineError::new(
                "exec output closed without a confirmed exit",
            ));
        }
        status
            .exit_code
            .ok_or_else(|| EngineError::new("exec exited without an exit code"))
    }
}
