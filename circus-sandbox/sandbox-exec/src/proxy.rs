//! Raw stdio proxying and host termination handling.
//!
//! Host stdin is read on a dedicated OS thread so a blocked read cannot keep the
//! Tokio runtime alive. Guest output is written on dedicated OS threads with
//! bounded channels. These threads carry bytes only; they never own cleanup.

#[cfg(not(unix))]
use std::future::Future;
use std::io::{Read, Write};
#[cfg(not(unix))]
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use circus_sandbox::{CleanupReport, ProcessExit, ProcessSession, ProcessTermination};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const CHUNK_BYTES: usize = 16 * 1024;
const CHANNEL_CHUNKS: usize = 8;
/// A guest that exits on stdin EOF must not be killed before it can flush.
const EOF_GRACE: Duration = Duration::from_secs(5);
const INPUT_STOP_WAIT: Duration = Duration::from_secs(2);
const OUTPUT_DRAIN_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    Signal(&'static str),
    StdinEof,
    InputFailed,
    OutputFailed,
}

impl ShutdownReason {
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::Signal(name) => name,
            Self::StdinEof => "stdin EOF",
            Self::InputFailed => "stdin failure",
            Self::OutputFailed => "host output failure",
        }
    }
}

#[derive(Debug)]
pub enum ProxyEnd {
    Exited {
        exit_code: i64,
    },
    Terminated {
        reason: ProcessTermination,
        cleanup: CleanupReport,
    },
    Shutdown(ShutdownReason),
    Lost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostEvent {
    StdinEof,
    InputFailed,
    OutputFailed,
}

enum StdinMessage {
    Data(Vec<u8>),
    Eof,
    Failed,
}

pub struct Proxy {
    session: Arc<ProcessSession>,
    events: mpsc::Receiver<HostEvent>,
    /// Keeps the channel open so a finished output task is distinguishable from
    /// an empty queue without a busy loop.
    _events: mpsc::Sender<HostEvent>,
    input_cancel: CancellationToken,
    input_task: Option<JoinHandle<()>>,
    output_task: Option<JoinHandle<()>>,
    stdout_writer: Option<std::thread::JoinHandle<()>>,
    stderr_writer: Option<std::thread::JoinHandle<()>>,
}

impl Proxy {
    #[must_use]
    pub fn start(session: Arc<ProcessSession>) -> Self {
        let (events, events_rx) = mpsc::channel(4);
        let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CHUNKS);
        let (stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CHUNKS);
        let stdout_writer = spawn_writer(stdout_rx, false);
        let stderr_writer = spawn_writer(stderr_rx, true);

        let output_events = events.clone();
        let output_session = Arc::clone(&session);
        let output_task = tokio::spawn(async move {
            while let Some(chunk) = output_session.next_output().await {
                let sent = match chunk {
                    circus_sandbox::ProcessOutput::Stdout(bytes) => stdout_tx.send(bytes).await,
                    circus_sandbox::ProcessOutput::Stderr(bytes) => stderr_tx.send(bytes).await,
                };
                if sent.is_err() {
                    let _ = output_events.send(HostEvent::OutputFailed).await;
                    return;
                }
            }
        });

        let input_cancel = CancellationToken::new();
        let input_events = events.clone();
        let input_session = Arc::clone(&session);
        let input_cancel_task = input_cancel.clone();
        let input_task = tokio::spawn(pump_stdin(input_session, input_cancel_task, input_events));

        Self {
            session,
            events: events_rx,
            _events: events,
            input_cancel,
            input_task: Some(input_task),
            output_task: Some(output_task),
            stdout_writer: Some(stdout_writer),
            stderr_writer: Some(stderr_writer),
        }
    }

    /// Waits for the guest, a host signal, or a host I/O failure.
    pub async fn next_end(&mut self, signals: &mut Signals) -> ProxyEnd {
        let session = Arc::clone(&self.session);
        let mut eof_deadline: Option<(tokio::time::Instant, ShutdownReason)> = None;
        loop {
            let timer_state = eof_deadline;
            let timer = async move {
                match timer_state {
                    Some((at, _)) => tokio::time::sleep_until(at).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                biased;
                reason = signals.next() => return ProxyEnd::Shutdown(reason),
                event = self.events.recv() => match event {
                    Some(HostEvent::OutputFailed) => {
                        return ProxyEnd::Shutdown(ShutdownReason::OutputFailed);
                    }
                    Some(HostEvent::StdinEof) => {
                        eof_deadline.get_or_insert_with(|| {
                            (
                                tokio::time::Instant::now() + EOF_GRACE,
                                ShutdownReason::StdinEof,
                            )
                        });
                    }
                    Some(HostEvent::InputFailed) => {
                        eof_deadline.get_or_insert_with(|| {
                            (
                                tokio::time::Instant::now() + EOF_GRACE,
                                ShutdownReason::InputFailed,
                            )
                        });
                    }
                    None => {}
                },
                () = timer => {
                    return ProxyEnd::Shutdown(eof_deadline.map_or(
                        ShutdownReason::InputFailed,
                        |(_, reason)| reason,
                    ));
                }
                exit = session.wait() => {
                    return match exit {
                        Ok(ProcessExit::Exited { exit_code }) => ProxyEnd::Exited { exit_code },
                        Ok(ProcessExit::Terminated { reason, cleanup }) => {
                            ProxyEnd::Terminated { reason, cleanup }
                        }
                        Err(_) => ProxyEnd::Lost,
                    };
                }
            }
        }
    }

    /// Signals the input task to stop after any in-flight write completes.
    /// Called before capture so an aborted write cannot request container
    /// destruction first.
    pub async fn stop_input(&mut self) {
        self.input_cancel.cancel();
        if let Some(mut task) = self.input_task.take()
            && tokio::time::timeout(INPUT_STOP_WAIT, &mut task)
                .await
                .is_err()
        {
            self.input_task = Some(task);
        }
    }

    /// Drains guest output and joins the writer threads without touching the
    /// input task. Safe to call before container destruction.
    pub async fn drain_output(&mut self) {
        if let Some(mut task) = self.output_task.take()
            && tokio::time::timeout(OUTPUT_DRAIN_WAIT, &mut task)
                .await
                .is_err()
        {
            task.abort();
            let _ = task.await;
        }
        if let Some(writer) = self.stdout_writer.take() {
            let _ = writer.join();
        }
        if let Some(writer) = self.stderr_writer.take() {
            let _ = writer.join();
        }
    }

    /// Drains output and aborts a still-blocked input task. Call only after
    /// cleanup has been attempted; aborting an in-flight input write requests
    /// container destruction through the session guard.
    pub async fn finish(&mut self) {
        self.drain_output().await;
        if let Some(task) = self.input_task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

async fn pump_stdin(
    session: Arc<ProcessSession>,
    cancel: CancellationToken,
    events: mpsc::Sender<HostEvent>,
) {
    let (sender, mut receiver) = mpsc::channel::<StdinMessage>(CHANNEL_CHUNKS);
    // This thread may stay blocked in a host read after shutdown. It owns no
    // cleanup or sandbox state and terminates with the host process.
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut stdin = stdin.lock();
        let mut buffer = vec![0u8; CHUNK_BYTES];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) => {
                    let _ = sender.blocking_send(StdinMessage::Eof);
                    return;
                }
                Ok(read) => {
                    if sender
                        .blocking_send(StdinMessage::Data(buffer[..read].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(_) => {
                    let _ = sender.blocking_send(StdinMessage::Failed);
                    return;
                }
            }
        }
    });

    loop {
        if cancel.is_cancelled() {
            return;
        }
        let message = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            message = receiver.recv() => message,
        };
        match message {
            Some(StdinMessage::Data(bytes)) => {
                if session.write_stdin(&bytes).await.is_err() {
                    let _ = events.send(HostEvent::InputFailed).await;
                    return;
                }
            }
            Some(StdinMessage::Eof) => {
                let event = if session.close_stdin().await.is_ok() {
                    HostEvent::StdinEof
                } else {
                    HostEvent::InputFailed
                };
                let _ = events.send(event).await;
                return;
            }
            Some(StdinMessage::Failed) | None => {
                let _ = events.send(HostEvent::InputFailed).await;
                return;
            }
        }
    }
}

fn spawn_writer(
    mut receiver: mpsc::Receiver<Vec<u8>>,
    stderr: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut output: Box<dyn Write> = if stderr {
            Box::new(std::io::stderr())
        } else {
            Box::new(std::io::stdout())
        };
        while let Some(bytes) = receiver.blocking_recv() {
            if output
                .write_all(&bytes)
                .and_then(|()| output.flush())
                .is_err()
            {
                return;
            }
        }
    })
}

/// Host termination sources. Unix signals are registered when this value is
/// constructed, before any backend work, so a signal during startup is not
/// handled by the default process action.
pub struct Signals {
    #[cfg(not(unix))]
    ctrl_c: Pin<Box<dyn Future<Output = std::io::Result<()>>>>,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl Signals {
    pub fn new() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            let interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            let terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            Ok(Self {
                interrupt,
                terminate,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                ctrl_c: Box::pin(tokio::signal::ctrl_c()),
            })
        }
    }

    pub async fn next(&mut self) -> ShutdownReason {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.interrupt.recv() => ShutdownReason::Signal("SIGINT"),
                _ = self.terminate.recv() => ShutdownReason::Signal("SIGTERM"),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (&mut self.ctrl_c).await;
            ShutdownReason::Signal("interrupt")
        }
    }
}
