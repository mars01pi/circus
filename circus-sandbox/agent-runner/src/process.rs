use std::io::Write;
use std::process::Stdio;

use circus_sandbox::{ProcessOutput, ProcessSession};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::events::{Events, MAX_EVENT_FRAME, WireEvent};
use crate::runner::guarded;
use crate::{RunnerConfig, TaskError};

pub(crate) async fn read_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
) -> Result<Option<Vec<u8>>, TaskError> {
    let mut frame = Vec::new();
    loop {
        let bytes = reader.fill_buf().await.map_err(TaskError::HostIo)?;
        if bytes.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(TaskError::Protocol)
            };
        }
        let newline = bytes.iter().position(|b| *b == b'\n');
        let length = newline.map_or(bytes.len(), |at| at + 1);
        if frame.len() + length > MAX_EVENT_FRAME {
            return Err(TaskError::Protocol);
        }
        frame.extend_from_slice(&bytes[..length]);
        reader.consume(length);
        if newline.is_some() {
            return Ok(Some(frame));
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn exchange(
    session: &ProcessSession,
    config: &RunnerConfig,
    prompt: &str,
    controller: &str,
    mcp_config: &str,
    token: &str,
    agent_session: Option<&str>,
    deadline: Instant,
    cancel: &CancellationToken,
    events: &mut Events,
) -> Result<(), TaskError> {
    let source = controller.to_owned();
    let script = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
        let mut script = tempfile::NamedTempFile::new()?;
        script.write_all(source.as_bytes())?;
        script.flush()?;
        Ok(script)
    })
    .await
    .map_err(|_| TaskError::WorkerGone)?
    .map_err(TaskError::HostIo)?;
    let mut command = tokio::process::Command::new(&config.python);
    command
        .args(["-I", "-B"])
        .arg(script.path())
        .env_clear()
        .env("CIRCUS_MODEL_BASE_URL", &config.model.base_url)
        .env("CIRCUS_MODEL", &config.model.model)
        .env("CIRCUS_MODEL_API_KEY", &config.model.api_key)
        .env("CIRCUS_TASK_TOKEN", token)
        .env("CIRCUS_AGENT_PROMPT", prompt)
        .env("CIRCUS_MCP_CONFIG", mcp_config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(path) = &config.model.ca_file {
        command.env("CIRCUS_MODEL_CA_FILE", path);
    }
    if let Some(id) = agent_session {
        command.env("CIRCUS_RESUME_THREAD", id);
    }
    for name in ["PATH", "SYSTEMROOT"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut child = command.spawn().map_err(TaskError::HostIo)?;
    // All three pipes were requested explicitly above.
    let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let (Some(input), Some(mut output), Some(diagnostics)) = pipes else {
        child.kill().await.map_err(TaskError::HostCleanup)?;
        child.wait().await.map_err(TaskError::HostCleanup)?;
        return Err(TaskError::Protocol);
    };
    let mut input = Some(input);
    let mut diagnostics = BufReader::new(diagnostics);
    let forward_input = async {
        let mut buffer = [0; 16 * 1024];
        loop {
            let count = output.read(&mut buffer).await.map_err(TaskError::HostIo)?;
            if count == 0 {
                break;
            }
            session
                .write_stdin(&buffer[..count])
                .await
                .map_err(|_| TaskError::Process)?;
        }
        // The process can already have exited. Its exit status is checked later.
        let _ = session.close_stdin().await;
        Ok::<_, TaskError>(())
    };
    let forward_output = async {
        while let Some(chunk) = session.next_output().await {
            if let ProcessOutput::Stdout(bytes) = chunk
                && let Some(writer) = input.as_mut()
                && writer.write_all(&bytes).await.is_err()
            {
                // Host failure diagnostics have priority over a secondary
                // broken pipe. Continue draining bounded guest output.
                input = None;
            }
        }
        drop(input);
        Ok::<_, TaskError>(())
    };
    let receive_events = async {
        let mut completed = false;
        while let Some(frame) = read_frame(&mut diagnostics).await? {
            let event: WireEvent = sonic_rs::from_slice(&frame).map_err(|_| TaskError::Protocol)?;
            completed |= event.apply(events)?;
        }
        if !completed {
            return Err(TaskError::Protocol);
        }
        Ok::<_, TaskError>(())
    };
    let transfer = guarded(
        async {
            tokio::try_join!(biased; receive_events, forward_input, forward_output).map(|_| ())
        },
        deadline,
        cancel,
    )
    .await
    .and_then(|result| result);
    let result = match transfer {
        Err(error) => Err(error),
        Ok(()) => match guarded(child.wait(), deadline, cancel).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(_)) => Err(TaskError::Process),
            Ok(Err(error)) => Err(TaskError::HostIo(error)),
            Err(error) => Err(error),
        },
    };
    if result.is_err() {
        if child.try_wait().map_err(TaskError::HostCleanup)?.is_none() {
            child.kill().await.map_err(TaskError::HostCleanup)?;
        }
        child.wait().await.map_err(TaskError::HostCleanup)?;
    }
    result
}
