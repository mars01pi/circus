//! Private, trusted Git preparation runner, not an adversarial process sandbox.
//!
//! Unix descendants must remain in their inherited group (no setsid escape).
//! Linux retains the unreaped leader until SIGKILL and a stable /proc census
//! establish that no group member can execute. ECHILD alone proves nothing about
//! grandchildren. Other Unix systems fail closed before spawning. Windows uses
//! process-wrap's suspended assignment and job completion port. No host-global
//! subreaper or signal disposition is installed beyond Tokio's SIGCHLD listener.

use crate::runtime::{ProcessCleanup, ProcessError};
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
#[cfg(target_os = "linux")]
use std::os::unix::process::ExitStatusExt;
use std::{
    ffi::OsString,
    io,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

pub(crate) struct GitCommand {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub environment: Vec<(OsString, OsString)>,
    pub timeout: Duration,
    pub output_limit: usize,
}

#[derive(Debug)]
pub(crate) struct ProcessOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

fn io_error(operation: &'static str, source: io::Error) -> ProcessError {
    ProcessError::Io { operation, source }
}

/// Even accidental cancellation of this future cannot drop its process owner.
/// The repository actor normally retains this future until completion.
pub(crate) async fn run_owned(
    command: GitCommand,
    cancellation: CancellationToken,
) -> Result<ProcessOutput, ProcessError> {
    let private_cancel = cancellation.child_token();
    let guard = private_cancel.clone().drop_guard();
    let (send, receive) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = send.send(run(command, private_cancel).await);
    });
    let result = receive
        .await
        .map_err(|_| io_error("owner task", io::Error::other("process owner task failed")))?;
    guard.disarm();
    result
}

async fn run(
    command: GitCommand,
    cancellation: CancellationToken,
) -> Result<ProcessOutput, ProcessError> {
    if !cfg!(any(target_os = "linux", windows)) {
        return Err(io_error(
            "platform",
            io::Error::new(
                io::ErrorKind::Unsupported,
                "process ownership requires Linux procfs or Windows jobs",
            ),
        ));
    }

    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    let deadline = tokio::time::Instant::now() + command.timeout;
    #[cfg(target_os = "linux")]
    let mut sigchld = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
        .map_err(|e| io_error("SIGCHLD subscription", e))?;
    let mut wrapped = CommandWrap::with_new(&command.executable, |cmd| {
        cmd.args(command.args)
            .current_dir(command.cwd)
            .env_clear()
            .envs(command.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    });
    wrapped.wrap(KillOnDrop);
    #[cfg(unix)]
    wrapped.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    wrapped.wrap(process_wrap::tokio::JobObject);
    let mut child = wrapped.spawn().map_err(|e| io_error("spawn", e))?;
    #[cfg(target_os = "linux")]
    let pid = child.id().expect("newly spawned child has an ID");
    let mut stdout = child.stdout().take().expect("piped stdout");
    let mut stderr = child.stderr().take().expect("piped stderr");
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let mut out_buf = [0_u8; 8192];
    let mut err_buf = [0_u8; 8192];
    let mut out_open = true;
    let mut err_open = true;
    let mut may_signal = true;
    #[cfg(target_os = "linux")]
    let mut observed: Option<ExitStatus> = None;
    let primary = loop {
        #[cfg(target_os = "linux")]
        match observe_leader(pid) {
            Ok(Some(status)) => {
                observed = Some(status);
                break None;
            }
            Ok(None) => {}
            Err(e) => {
                may_signal = false;
                break Some(io_error("observe leader", e));
            }
        }
        #[cfg(windows)]
        let observation = child.inner_mut().wait();
        #[cfg(target_os = "linux")]
        let observation = async {
            sigchld.recv().await;
            Ok::<(), io::Error>(())
        };
        #[cfg(not(any(target_os = "linux", windows)))]
        let observation = std::future::pending::<io::Result<()>>();
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => break Some(ProcessError::Cancelled),
            _ = tokio::time::sleep_until(deadline) => break Some(ProcessError::Timeout),
            result = observation => {
                if let Err(error) = result { break Some(io_error("observe leader", error)); }
                #[cfg(windows)]
                break None;
            },
            read = stdout.read(&mut out_buf), if out_open => match capture(read, &out_buf, &mut output, errors.len(), command.output_limit) {
                Ok(open) => out_open = open,
                Err(e) => break Some(e),
            },
            read = stderr.read(&mut err_buf), if err_open => match capture(read, &err_buf, &mut errors, output.len(), command.output_limit) {
                Ok(open) => err_open = open,
                Err(e) => break Some(e),
            },
        }
    };
    // Never wait/reap the Unix leader before the last group signal. Its zombie
    // pins the PGID, preventing a late kill from targeting a reused identity.
    match terminate(
        &mut *child,
        &mut may_signal,
        #[cfg(target_os = "linux")]
        pid,
        #[cfg(target_os = "linux")]
        observed,
    )
    .await
    {
        Ok(status) => {
            if let Some(error) = primary {
                return Err(error);
            }
            // All writers are dead, so remaining pipe bytes are finite. Continue
            // enforcing one aggregate budget, including bytes buffered at exit.
            while out_open || err_open {
                tokio::select! {
                    read = stdout.read(&mut out_buf), if out_open => out_open = capture(read, &out_buf, &mut output, errors.len(), command.output_limit)?,
                    read = stderr.read(&mut err_buf), if err_open => err_open = capture(read, &err_buf, &mut errors, output.len(), command.output_limit)?,
                }
            }
            Ok(ProcessOutput {
                status,
                stdout: output,
                stderr: errors,
            })
        }
        Err(source) => {
            let (done, receive) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                // Once wrapper waiting begins, a failed wait may have reaped
                // the leader. Never signal its cached PGID again in that phase.
                #[cfg(target_os = "linux")]
                loop {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if !may_signal {
                        std::future::pending::<()>().await;
                    }
                    if terminate(&mut *child, &mut may_signal, pid, observed)
                        .await
                        .is_ok()
                    {
                        let _ = done.send(true);
                        break;
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _retained = (child, done);
                    std::future::pending::<()>().await;
                }
            });
            Err(ProcessError::Cleanup {
                primary: primary.map(Box::new),
                source,
                cleanup: ProcessCleanup { done: receive },
            })
        }
    }
}

/// `try_wait` can report a vanished process group as ESRCH/ECHILD; that means
/// no status is cached, not that the caller failed.
#[cfg(target_os = "linux")]
fn try_status(child: &mut dyn ChildWrapper) -> io::Result<Option<ExitStatus>> {
    match child.try_wait() {
        Ok(status) => Ok(status),
        Err(error) if matches!(error.raw_os_error(), Some(3) | Some(10)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn capture(
    read: io::Result<usize>,
    buffer: &[u8],
    target: &mut Vec<u8>,
    other: usize,
    limit: usize,
) -> Result<bool, ProcessError> {
    let count = read.map_err(|e| io_error("read output", e))?;
    if count > limit.saturating_sub(target.len()).saturating_sub(other) {
        return Err(ProcessError::OutputLimit { limit });
    }
    target.extend_from_slice(&buffer[..count]);
    Ok(count != 0)
}

/// Observes leader exit without reaping it.
///
/// The zombie keeps the process group identity valid until the group is killed,
/// and its status is the fallback when the group vanishes before the runtime
/// wait runs.
#[cfg(target_os = "linux")]
fn observe_leader(pid: u32) -> io::Result<Option<ExitStatus>> {
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    let pid = Pid::from_raw(pid as i32).ok_or_else(|| io::Error::other("invalid child PID"))?;
    let status = waitid(
        WaitId::Pid(pid),
        WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
    )
    .map_err(io::Error::from)?;
    Ok(status.and_then(|status| {
        if let Some(code) = status.exit_status() {
            Some(ExitStatus::from_raw(code << 8))
        } else {
            status.terminating_signal().map(ExitStatus::from_raw)
        }
    }))
}

async fn terminate(
    child: &mut dyn ChildWrapper,
    may_signal: &mut bool,
    #[cfg(target_os = "linux")] pid: u32,
    #[cfg(target_os = "linux")] observed: Option<ExitStatus>,
) -> io::Result<ExitStatus> {
    if !*may_signal {
        return Err(io::Error::other(
            "process identity is no longer safe to signal",
        ));
    }
    #[cfg(target_os = "linux")]
    if let Err(error) = observe_leader(pid) {
        *may_signal = false;
        // The leader can disappear between the loop and this check; the
        // status observed earlier is then the only honest answer.
        if matches!(error.raw_os_error(), Some(3) | Some(10)) {
            if let Some(status) = observed {
                return Ok(status);
            }
            if let Some(status) = try_status(child)? {
                return Ok(status);
            }
        }
        return Err(error);
    }
    // `killpg` reports ESRCH (3) when the whole group is already gone; that is
    // the outcome termination wants, not a cleanup failure.
    #[cfg(target_os = "linux")]
    if let Err(error) = child.start_kill()
        && error.raw_os_error() != Some(3)
    {
        return Err(error);
    }
    #[cfg(not(target_os = "linux"))]
    child
        .start_kill()
        .map_err(|e| io::Error::new(e.kind(), format!("kill group: {e}")))?;
    #[cfg(target_os = "linux")]
    {
        let stop = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match group_stopped(pid).await {
                Ok(true) => break,
                Ok(false) => {}
                Err(error) => {
                    return Err(error);
                }
            }
            if tokio::time::Instant::now() >= stop {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "process group still has executing members",
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    *may_signal = false;
    #[cfg(target_os = "linux")]
    {
        use rustix::io::Errno;
        use rustix::process::{Pid, WaitOptions, waitpid};
        let leader =
            Pid::from_raw(pid as i32).ok_or_else(|| io::Error::other("invalid child PID"))?;
        match waitpid(Some(leader), WaitOptions::empty()) {
            Ok(Some((_, status))) => return Ok(ExitStatus::from_raw(status.as_raw())),
            Ok(None) => {}
            Err(Errno::CHILD | Errno::SRCH) => {}
            Err(error) => {
                return Err(error.into());
            }
        }
        // Another runtime path reaped the leader first; use the status observed
        // by `waitid(WNOWAIT)` or the runtime cache.
        if let Some(status) = observed {
            return Ok(status);
        }
        if let Some(status) = try_status(child)? {
            return Ok(status);
        }
        Err(io::Error::other("leader exit status is unavailable"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        child.wait().await
    }
}

#[cfg(target_os = "linux")]
async fn group_stopped(pgid: u32) -> io::Result<bool> {
    // Two identical full PID censuses bracket the state inspection. If a live
    // member forks while being inspected, the second census changes; if every
    // member is zombie/dead, no member can create a new group participant.
    async fn pids() -> io::Result<Vec<u32>> {
        let mut entries = tokio::fs::read_dir("/proc").await?;
        let mut result = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) {
                result.push(pid);
            }
        }
        result.sort_unstable();
        Ok(result)
    }
    let before = pids().await?;
    for pid in &before {
        let stat = match tokio::fs::read_to_string(format!("/proc/{pid}/stat")).await {
            Ok(stat) => stat,
            // A process that exits mid-read makes the kernel report ESRCH
            // (or NotFound); take a fresh census instead of failing.
            Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(3) => {
                return Ok(false);
            }
            Err(e) => return Err(e),
        };
        // A process that exits while its record is read leaves a truncated
        // /proc entry. That is a changed census, not a protocol failure: report
        // "not stopped" and let the caller take another census.
        let Some(tail) = stat.rsplit_once(") ").map(|(_, tail)| tail) else {
            return Ok(false);
        };
        let mut fields = tail.split_ascii_whitespace();
        let Some(state) = fields.next() else {
            return Ok(false);
        };
        let _parent = fields.next();
        let Some(group) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            return Ok(false);
        };
        if group == pgid && state != "Z" && state != "X" {
            return Ok(false);
        }
    }
    Ok(before == pids().await?)
}

#[cfg(test)]
mod tests;
