//! FIFO plumbing for containerd task exec.
//!
//! containerd's runc shim attaches a process through named pipes whose paths
//! are supplied in the exec request. This module creates them and opens the
//! host ends without blocking the async runtime.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use containerd_client::services::v1::{CloseIoRequest, tasks_client::TasksClient};
use containerd_client::tonic::Request;
use containerd_client::tonic::transport::Channel;
use containerd_client::with_namespace;
use rustix::fs::{FileType, Mode, OFlags};
use tokio::io::AsyncWrite;

use super::containerd_grpc::local_error;
use super::engine::EngineError;

/// One temporary directory holding a stdin/stdout/stderr FIFO triple.
///
/// The directory is unlinked on drop. A process attachment keeps this value
/// alive until its transport ends.
pub(crate) struct FifoDir {
    dir: tempfile::TempDir,
    stdin: PathBuf,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl FifoDir {
    pub(crate) fn create() -> Result<Self, EngineError> {
        let dir = tempfile::tempdir()
            .map_err(|err| local_error(format!("containerd exec temp directory: {err}")))?;
        let stdin = dir.path().join("stdin");
        let stdout = dir.path().join("stdout");
        let stderr = dir.path().join("stderr");
        for path in [&stdin, &stdout, &stderr] {
            make_fifo(path)?;
        }
        Ok(Self {
            dir,
            stdin,
            stdout,
            stderr,
        })
    }

    pub(crate) fn stdin(&self) -> String {
        path_text(&self.stdin)
    }

    pub(crate) fn stdout(&self) -> String {
        path_text(&self.stdout)
    }

    pub(crate) fn stderr(&self) -> String {
        path_text(&self.stderr)
    }

    pub(crate) fn stdout_path(&self) -> PathBuf {
        self.stdout.clone()
    }

    pub(crate) fn stderr_path(&self) -> PathBuf {
        self.stderr.clone()
    }

    pub(crate) fn stdin_path(&self) -> PathBuf {
        self.stdin.clone()
    }

    /// Keeps the directory alive for a process transport.
    pub(crate) fn into_guard(self) -> tempfile::TempDir {
        self.dir
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn make_fifo(path: &Path) -> Result<(), EngineError> {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        FileType::Fifo,
        Mode::RUSR | Mode::WUSR,
        0,
    )
    .map_err(|err| local_error(format!("containerd exec fifo: {err}")))
}

/// Opens the reader end. This blocks until a writer attaches, so it must run on
/// a blocking thread concurrently with `Start` (see [`open_reader_async`]).
pub(crate) fn open_reader(path: PathBuf) -> Result<tokio::fs::File, EngineError> {
    let fd = rustix::fs::open(&path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .map_err(|err| local_error(format!("containerd exec reader: {err}")))?;
    Ok(tokio::fs::File::from_std(std::fs::File::from(fd)))
}

/// Opens the writer end read-write so it never blocks on a missing peer.
///
/// The shim keeps its own stdin descriptor until `CloseIO`; [`FifoWriter`]
/// therefore closes both this descriptor and the shim's stdin on shutdown.
pub(crate) fn open_writer(path: &Path, close: CloseIo) -> Result<FifoWriter, EngineError> {
    let fd = rustix::fs::open(path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
        .map_err(|err| local_error(format!("containerd exec writer: {err}")))?;
    Ok(FifoWriter {
        file: Some(tokio::fs::File::from_std(std::fs::File::from(fd))),
        close: Some(close),
    })
}

/// Identity needed to tell the shim that stdin reached EOF.
pub(crate) struct CloseIo {
    channel: Channel,
    namespace: String,
    container_id: String,
    exec_id: String,
}

impl CloseIo {
    pub(crate) fn new(
        channel: Channel,
        namespace: &str,
        container_id: &str,
        exec_id: &str,
    ) -> Self {
        Self {
            channel,
            namespace: namespace.to_string(),
            container_id: container_id.to_string(),
            exec_id: exec_id.to_string(),
        }
    }

    /// Sends `CloseIO` on a runtime handle. A missing runtime cannot be waited
    /// for here; the descriptor close still happens, and lifecycle destruction
    /// removes the process.
    fn spawn(self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            let mut client = TasksClient::new(self.channel);
            let request = with_namespace!(
                CloseIoRequest {
                    container_id: self.container_id,
                    exec_id: self.exec_id,
                    stdin: true,
                },
                self.namespace.as_str()
            );
            let _ = client.close_io(request).await;
        });
    }
}

/// Write end of a guest stdin FIFO that actually closes on shutdown.
pub(crate) struct FifoWriter {
    file: Option<tokio::fs::File>,
    close: Option<CloseIo>,
}

impl Drop for FifoWriter {
    fn drop(&mut self) {
        if let Some(close) = self.close.take() {
            close.spawn();
        }
    }
}

impl AsyncWrite for FifoWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut().file.as_mut() {
            Some(file) => Pin::new(file).poll_write(cx, buffer),
            None => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "guest stdin is closed",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut().file.as_mut() {
            Some(file) => Pin::new(file).poll_flush(cx),
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match this.file.as_mut() {
            Some(file) => match Pin::new(file).poll_flush(cx) {
                Poll::Ready(Ok(())) => {
                    // Closing our descriptor plus the shim's stdin is the only
                    // way a containerd exec observes EOF.
                    this.file = None;
                    if let Some(close) = this.close.take() {
                        close.spawn();
                    }
                    Poll::Ready(Ok(()))
                }
                other => other,
            },
            None => Poll::Ready(Ok(())),
        }
    }
}

/// Makes a blocked reader open return when a start request failed.
///
/// Opening the write end unblocks a reader waiting for a writer; closing it
/// immediately makes the reader observe EOF.
pub(crate) fn unblock_readers(paths: [&Path; 2]) {
    for path in paths {
        if let Ok(fd) = rustix::fs::open(path, OFlags::WRONLY | OFlags::NONBLOCK, Mode::empty()) {
            drop(fd);
        }
    }
}

/// Opens a reader end on the blocking pool, concurrently with `Start`.
pub(crate) async fn open_reader_async(path: PathBuf) -> Result<tokio::fs::File, EngineError> {
    tokio::task::spawn_blocking(move || open_reader(path))
        .await
        .map_err(|err| local_error(format!("containerd exec reader task: {err}")))?
}
