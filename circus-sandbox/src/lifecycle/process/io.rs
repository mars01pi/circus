//! Bounded process transport; no lifecycle state mutations or container deletion.

use std::time::Duration;

use futures_util::{Stream, StreamExt};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{CHUNK_BYTES, Input, ProcessError, ProcessOutput, ProcessTermination};
use crate::infrastructure::{BackendProcess, EngineError, ProcessChunk};

pub(super) async fn run(
    process: BackendProcess,
    input: mpsc::Receiver<Input>,
    output: mpsc::Sender<ProcessOutput>,
    cancel: CancellationToken,
    timeout: Duration,
    limit: usize,
) -> Result<i64, ProcessTermination> {
    let BackendProcess {
        input: writer,
        output: reader,
        exit,
    } = process;
    let send = write_input(writer, input);
    let receive = async {
        read_output(reader, output, limit).await?;
        // The backend confirms the guest status only after its output stream
        // has ended; an unconfirmed end is a transport failure, not an exit.
        exit.await.map_err(|err| ProcessTermination::Transport {
            detail: err.message,
        })
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ProcessTermination::Cancelled),
        () = tokio::time::sleep(timeout) => Err(ProcessTermination::TimedOut),
        result = send => Err(result),
        result = receive => result,
    }
}

async fn write_input(
    mut writer: impl AsyncWrite + Unpin,
    mut input: mpsc::Receiver<Input>,
) -> ProcessTermination {
    let mut closed = false;
    while let Some(request) = input.recv().await {
        let (result, reply) = match request {
            Input::Write(bytes, reply) => {
                if closed {
                    let _ = reply.send(Err(ProcessError::StdinClosed));
                    continue;
                }
                let result = async {
                    writer.write_all(&bytes).await?;
                    writer.flush().await
                }
                .await;
                (result, reply)
            }
            Input::Close(reply) => {
                let result = if closed {
                    Ok(())
                } else {
                    writer.shutdown().await
                };
                closed = true;
                (result, reply)
            }
        };
        if let Err(err) = result {
            let detail = err.to_string();
            let _ = reply.send(Err(ProcessError::Io {
                detail: detail.clone(),
            }));
            return ProcessTermination::Transport { detail };
        }
        if reply.send(Ok(())).is_err() {
            return ProcessTermination::Cancelled;
        }
    }
    ProcessTermination::Cancelled
}

async fn read_output(
    mut reader: impl Stream<Item = Result<ProcessChunk, EngineError>> + Unpin,
    output: mpsc::Sender<ProcessOutput>,
    limit: usize,
) -> Result<(), ProcessTermination> {
    let mut stdout = 0usize;
    let mut stderr = 0usize;
    while let Some(chunk) = reader.next().await {
        let chunk = chunk.map_err(|err| ProcessTermination::Transport {
            detail: err.message,
        })?;
        let (bytes, total, is_stderr) = match chunk {
            ProcessChunk::Stdout(bytes) => (bytes, &mut stdout, false),
            ProcessChunk::Stderr(bytes) => (bytes, &mut stderr, true),
        };
        if bytes.len() > limit.saturating_sub(*total) {
            return Err(ProcessTermination::OutputLimit { limit });
        }
        *total += bytes.len();
        for part in bytes.chunks(CHUNK_BYTES) {
            let event = if is_stderr {
                ProcessOutput::Stderr(part.to_vec())
            } else {
                ProcessOutput::Stdout(part.to_vec())
            };
            output
                .send(event)
                .await
                .map_err(|_| ProcessTermination::Cancelled)?;
        }
    }
    Ok(())
}
