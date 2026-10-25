//! Attached interactive process over `pods/exec`.
//!
//! Kubernetes exec has no environment or working-directory option, so the
//! configured argv runs through a positional Python trampoline: it changes
//! directory, merges the explicit environment overrides over the image
//! environment, and `execvpe`s the user command. The user command is never
//! parsed as Python source; it is passed as argv after a standalone `--`
//! sentinel. Because `pods/exec` carries argv rather than an environment block,
//! override values are part of the exec request and the trampoline's argument
//! list while it starts; after `exec` they live in the guest environment, which
//! same-uid processes in the sandbox can read like any image environment
//! variable. This adapter never logs argv or environment values.
//!
//! Ownership and ordering. The `kube` attached process is owned by the exit
//! future, so dropping that future aborts the kube message loop through its
//! `Drop`, while the output stream owns only the stdout/stderr readers and
//! spawns no reader task. Output must end before exit is awaited: exit joins
//! the transport first and only then maps the status channel. Dropping the
//! output stream stops reads; lifecycle still owns cancellation and cleanup.

use futures_util::stream::{self, Stream};
use kube::api::AttachParams;
use tokio::io::{AsyncRead, AsyncReadExt};

use super::backend::BackendProcess;
use super::engine::{EngineError, EngineErrorKind, ProcessChunk, ProcessSpec};
use super::kubernetes_engine::{KubernetesEngine, engine_error};
use super::kubernetes_io::{ATTACH_BUFFER_BYTES, exit_code, stream_error};

/// One read per output stream tracks the 64 KiB attach buffers without
/// materializing more than one bounded chunk per poll.
const READ_CHUNK_BYTES: usize = 16 * 1024;

/// Positional trampoline. `sys.argv` is `-c`, `--`, workdir, override count,
/// `KEY=VALUE` overrides, then the user command. Overrides are split at the
/// first `=`, so values may contain `=`. Failures before `exec` use the same
/// `125`/`127` codes as a shell would, without invoking one.
const PROCESS_EXEC: &str = "import os,sys\na=sys.argv\ntry:\n    s=a.index(\"--\")\nexcept ValueError:\n    raise SystemExit(125)\nw=a[s+1]\nn=int(a[s+2])\ne=a[s+3:s+3+n]\nc=a[s+3+n:]\ntry:\n    os.chdir(w)\nexcept OSError as x:\n    sys.stderr.write(f\"circus: cannot change directory: {x}\\n\")\n    raise SystemExit(125)\nv=dict(os.environ)\nfor i in e:\n    k,_,x=i.partition(\"=\")\n    v[k]=x\ntry:\n    os.execvpe(c[0],c,v)\nexcept OSError as x:\n    sys.stderr.write(f\"circus: cannot execute: {x}\\n\")\n    raise SystemExit(127)";

impl KubernetesEngine {
    /// Attaches one interactive process to the Pod's single container.
    ///
    /// The `user` field is inert: `pods/exec` cannot change identity, and the
    /// container security context already fixed the guest uid/gid.
    pub(super) async fn spawn_process(
        &self,
        name: &str,
        spec: ProcessSpec<'_>,
    ) -> Result<BackendProcess, EngineError> {
        let command = process_argv(spec.workdir, spec.argv, &spec.env);
        let params = AttachParams {
            stdin: true,
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
            .exec(name, command, &params)
            .await
            .map_err(|err| {
                engine_error(
                    EngineErrorKind::Protocol,
                    format!("pods/exec attach failed: {err}"),
                )
            })?;
        let input = process
            .stdin()
            .ok_or_else(|| engine_error(EngineErrorKind::Protocol, "exec did not attach stdin"))?;
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
        let output = merged_output(stdout, stderr);
        // The exit future owns the transport. Lifecycle awaits it only after
        // the output stream ends; dropping it aborts the kube message loop.
        let exit = async move {
            process.join().await.map_err(|err| {
                engine_error(
                    EngineErrorKind::Protocol,
                    format!("pods/exec stream failed: {err}"),
                )
            })?;
            Ok(exit_code(status.await.as_ref()))
        };
        Ok(BackendProcess {
            input: Box::pin(input),
            output: Box::pin(output),
            exit: Box::pin(exit),
        })
    }
}

/// Builds the trampoline argv without interpolating any caller value into the
/// Python source. Environment entries stay single argv elements; the command
/// follows the standalone `--` sentinel and is never parsed by Python.
fn process_argv(workdir: &str, argv: &[String], env: &[String]) -> Vec<String> {
    let mut command = Vec::with_capacity(8 + env.len() + argv.len());
    command.extend([
        "python3".to_string(),
        "-I".to_string(),
        "-B".to_string(),
        "-c".to_string(),
        PROCESS_EXEC.to_string(),
        "--".to_string(),
        workdir.to_string(),
        env.len().to_string(),
    ]);
    command.extend(env.iter().cloned());
    command.extend(argv.iter().cloned());
    command
}

struct OutputState<O, E> {
    stdout: O,
    stderr: E,
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
    stdout_done: bool,
    stderr_done: bool,
    failed: bool,
}

/// Merges stdout and stderr in per-stream order; interleaving between the two
/// streams is the transport's order. `unfold` keeps the readers in the stream
/// state, so dropping the stream stops reads without a detached reader task.
fn merged_output<O, E>(
    stdout: O,
    stderr: E,
) -> impl Stream<Item = Result<ProcessChunk, EngineError>> + Send
where
    O: AsyncRead + Unpin + Send + 'static,
    E: AsyncRead + Unpin + Send + 'static,
{
    let state = OutputState {
        stdout,
        stderr,
        stdout_buf: vec![0u8; READ_CHUNK_BYTES],
        stderr_buf: vec![0u8; READ_CHUNK_BYTES],
        stdout_done: false,
        stderr_done: false,
        failed: false,
    };
    stream::unfold(state, |state| async move {
        let OutputState {
            mut stdout,
            mut stderr,
            mut stdout_buf,
            mut stderr_buf,
            mut stdout_done,
            mut stderr_done,
            mut failed,
        } = state;
        loop {
            if failed || (stdout_done && stderr_done) {
                return None;
            }
            let item: Option<Result<ProcessChunk, EngineError>> = tokio::select! {
                read = stdout.read(&mut stdout_buf), if !stdout_done => {
                    match read {
                        Ok(0) => {
                            stdout_done = true;
                            None
                        }
                        Ok(len) => Some(Ok(ProcessChunk::Stdout(stdout_buf[..len].to_vec()))),
                        Err(error) => {
                            failed = true;
                            Some(Err(stream_error(error)))
                        }
                    }
                }
                read = stderr.read(&mut stderr_buf), if !stderr_done => {
                    match read {
                        Ok(0) => {
                            stderr_done = true;
                            None
                        }
                        Ok(len) => Some(Ok(ProcessChunk::Stderr(stderr_buf[..len].to_vec()))),
                        Err(error) => {
                            failed = true;
                            Some(Err(stream_error(error)))
                        }
                    }
                }
            };
            if let Some(item) = item {
                return Some((
                    item,
                    OutputState {
                        stdout,
                        stderr,
                        stdout_buf,
                        stderr_buf,
                        stdout_done,
                        stderr_done,
                        failed,
                    },
                ));
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use futures_util::StreamExt;
    use tokio::io::ReadBuf;

    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn trampoline_argv_is_positional_and_never_interpolated() {
        let argv = strings(&["python3", "-c", "print('; rm -rf /')", "--workdir", "$(id)"]);
        let env = strings(&[
            "PATH=/usr/bin:/bin",
            "CIRCUS_FLAG=left=right",
            "CIRCUS_RAW=$(touch /tmp/pwned);`id`",
        ]);
        let command = process_argv("/workspace/a;b", &argv, &env);

        assert_eq!(command[0], "python3");
        assert_eq!(command[1], "-I");
        assert_eq!(command[2], "-B");
        assert_eq!(command[3], "-c");
        assert_eq!(command[4], PROCESS_EXEC);
        assert_eq!(command[5], "--");
        assert_eq!(command[6], "/workspace/a;b");
        assert_eq!(command[7], "3");
        assert_eq!(&command[8..11], env.as_slice());
        assert_eq!(&command[11..], argv.as_slice());
        assert_eq!(command.len(), 8 + env.len() + argv.len());
        // No interpolation into the Python source and no shell execution path.
        for value in env.iter().chain(argv.iter()) {
            assert!(!PROCESS_EXEC.contains(value.as_str()));
        }
        assert!(!PROCESS_EXEC.contains("subprocess"));
        assert!(!PROCESS_EXEC.contains("os.system"));
        assert!(!PROCESS_EXEC.contains("shell"));
    }

    #[test]
    fn trampoline_uses_a_sentinel_and_argv_only_environment_mapping() {
        assert!(PROCESS_EXEC.contains("a.index(\"--\")"));
        assert!(PROCESS_EXEC.contains("os.chdir(w)"));
        assert!(PROCESS_EXEC.contains("i.partition(\"=\")"));
        assert!(PROCESS_EXEC.contains("os.execvpe(c[0],c,v)"));

        let argv = strings(&["echo", "--", "tail"]);
        let command = process_argv("/workspace", &argv, &[]);
        assert_eq!(command[7], "0");
        assert_eq!(&command[8..], argv.as_slice());
        // The argv's own `--` never moves the sentinel: the first sentinel is
        // still the fixed one at index 5, before the workdir.
        assert_eq!(command.iter().position(|value| value == "--"), Some(5));
    }

    #[test]
    fn trampoline_positions_match_the_built_argv() {
        let env = strings(&["A=1", "B=x=y", "EMPTY="]);
        let argv = strings(&["/bin/echo", "hi", "--", "there"]);
        let command = process_argv("/workspace/project", &argv, &env);

        // Mirrors the trampoline's positional parsing exactly.
        let sentinel = command
            .iter()
            .position(|value| value == "--")
            .expect("standalone sentinel");
        let workdir = &command[sentinel + 1];
        let count: usize = command[sentinel + 2].parse().expect("override count");
        let overrides = &command[sentinel + 3..sentinel + 3 + count];
        let user = &command[sentinel + 3 + count..];

        assert_eq!(workdir, "/workspace/project");
        assert_eq!(count, env.len());
        assert_eq!(overrides, env.as_slice());
        assert_eq!(user, argv.as_slice());
    }

    #[tokio::test]
    async fn merged_output_preserves_per_stream_order() {
        let stream = merged_output(&b"alpha"[..], &b"beta"[..]);
        let chunks: Vec<_> = stream.collect().await;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        for chunk in chunks {
            match chunk.expect("in-memory reads cannot fail") {
                ProcessChunk::Stdout(bytes) => stdout.extend(bytes),
                ProcessChunk::Stderr(bytes) => stderr.extend(bytes),
            }
        }
        assert_eq!(stdout, b"alpha");
        assert_eq!(stderr, b"beta");
    }

    struct Failing;

    impl AsyncRead for Failing {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other("socket closed")))
        }
    }

    #[tokio::test]
    async fn merged_output_maps_reader_failure_to_a_typed_protocol_error() {
        let stream = merged_output(Failing, &b""[..]);
        let chunks: Vec<_> = stream.collect().await;
        match chunks.as_slice() {
            [Err(error)] => {
                assert_eq!(error.kind, EngineErrorKind::Protocol);
                assert!(error.message.contains("socket closed"));
            }
            _ => panic!("expected exactly one protocol error"),
        }
    }
}
