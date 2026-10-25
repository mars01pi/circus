use std::time::Duration;

use circus_agent_runner::{AgentRunner, AgentTask, CleanupHandle};

use crate::io::{Incoming, Output};
use crate::protocol::{Cleanup, Failure, FinalResult, MAX_FRAME, Request, Response, VERSION};

pub async fn serve(
    runner: AgentRunner,
    mut input: Incoming,
    output: &Output,
) -> Result<(), &'static str> {
    output
        .send(&Response::Ready {
            v: VERSION,
            max_frame_bytes: MAX_FRAME,
        })
        .await?;
    let signal = shutdown_signal();
    tokio::pin!(signal);
    let first = tokio::select! {
        request = input.recv() => request,
        _ = &mut signal => return Ok(()),
    };
    let Some(first) = first else {
        return Ok(());
    };
    let Request::Start {
        request_id,
        request,
        ..
    } = first?
    else {
        return Err("invalid_state");
    };
    let grants = request.mcp_grants.clone();
    let include_agent_session =
        request.workspace_archive.is_some() || request.agent_session.is_some();
    let task = runner
        .start_with_mcp(request.into_task()?, grants)
        .map_err(|_| "invalid_request")?;
    let accepted = Response::Accepted {
        v: VERSION,
        request_id: &request_id,
        task_id: task.id(),
    };
    if let Err(error) = output.send(&accepted).await {
        cancel_and_settle(&task).await;
        return Err(error);
    }
    let mut seq = 0;
    let mut connection_error = None;
    let mut disconnected = false;
    loop {
        tokio::select! {
            biased;
            frame = input.recv() => {
                let control = match frame {
                    None => { disconnected = true; break; },
                    Some(Err(error)) => Err(error),
                    Some(Ok(frame)) => cancel_frame(frame, &request_id, &task, output).await,
                };
                if let Err(error) = control { connection_error = Some(error); break; }
            }
            _ = &mut signal => { disconnected = true; break; }
            event = task.next_event() => {
                let Some(event) = event else { break; };
                seq += 1;
                if let Err(error) = output.send(&Response::Event { v: VERSION, request_id: &request_id, task_id: task.id(), seq, event: event.into() }).await {
                    connection_error = Some(error); break;
                }
            }
        }
    }
    if disconnected || connection_error.is_some() {
        task.cancel();
    }
    let outcome = task.wait().await;
    let mut cleanup_handle = outcome
        .as_ref()
        .as_ref()
        .err()
        .and_then(|failure| failure.cleanup.clone());
    let (mut result, mut cleanup) =
        crate::outcome::present(outcome.as_ref(), include_agent_session);
    let after_retry = crate::outcome::after_cleanup_retry(outcome.as_ref());
    if disconnected && outcome.is_ok() {
        result = FinalResult::Cancelled {
            cleanup,
            error: Failure {
                code: "cancelled",
                message: "Client disconnected before the result was delivered",
            },
        };
    }
    if let Some(error) = connection_error {
        retry(&mut cleanup_handle, &mut cleanup, after_retry).await;
        return Err(error);
    }
    if output
        .send(&Response::Result {
            v: VERSION,
            request_id: &request_id,
            task_id: task.id(),
            result,
        })
        .await
        .is_err()
    {
        retry(&mut cleanup_handle, &mut cleanup, after_retry).await;
        return Err("transport");
    }
    if !disconnected {
        loop {
            let frame = tokio::select! {
                frame = input.recv() => frame,
                _ = &mut signal => None,
            };
            let Some(frame) = frame else {
                break;
            };
            let operation = match frame {
                Ok(Request::CleanupRetry {
                    request_id: owner,
                    task_id,
                    ..
                }) if owner == request_id && task_id == task.id() => {
                    retry(&mut cleanup_handle, &mut cleanup, after_retry).await;
                    output
                        .send(&Response::CleanupResult {
                            v: VERSION,
                            request_id: &request_id,
                            task_id: task.id(),
                            cleanup,
                        })
                        .await
                }
                Ok(frame) => cancel_frame(frame, &request_id, &task, output).await,
                Err(error) => Err(error),
            };
            if let Err(error) = operation {
                retry(&mut cleanup_handle, &mut cleanup, after_retry).await;
                return Err(error);
            }
        }
    }
    retry(&mut cleanup_handle, &mut cleanup, after_retry).await;
    if cleanup == Cleanup::Confirmed {
        Ok(())
    } else {
        Err("transport")
    }
}

async fn cancel_frame(
    frame: Request,
    owner: &str,
    task: &AgentTask,
    output: &Output,
) -> Result<(), &'static str> {
    match frame {
        Request::Cancel {
            request_id,
            task_id,
            ..
        } if request_id == owner && task_id == task.id() => {
            task.cancel();
            output
                .send(&Response::CancelAck {
                    v: VERSION,
                    request_id: owner,
                    task_id: task.id(),
                })
                .await
        }
        Request::Start { .. } => Err("invalid_state"),
        Request::CleanupRetry {
            request_id,
            task_id,
            ..
        } if request_id == owner && task_id == task.id() => Err("invalid_state"),
        _ => Err("wrong_task"),
    }
}

async fn cancel_and_settle(task: &AgentTask) {
    task.cancel();
    let outcome = task.wait().await;
    if let Err(failure) = outcome.as_ref() {
        let mut handle = failure.cleanup.clone();
        let mut status = Cleanup::Pending;
        retry(
            &mut handle,
            &mut status,
            crate::outcome::after_cleanup_retry(outcome.as_ref()),
        )
        .await;
    }
}

async fn retry(handle: &mut Option<CleanupHandle>, status: &mut Cleanup, after_retry: Cleanup) {
    let Some(owner) = handle.as_ref() else {
        return;
    };
    if matches!(
        tokio::time::timeout(Duration::from_secs(35), owner.retry()).await,
        Ok(Ok(()))
    ) {
        *handle = None;
        *status = after_retry;
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
