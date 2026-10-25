use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{PoolConfig, PoolError, PoolFailure, PoolLease, PoolStatus};
use crate::observability::{SandboxMetrics, events};
use crate::{CleanupReceipt, Sandbox, SandboxError, SandboxPhase};

type Reply = oneshot::Sender<Result<(), PoolError>>;
type Failure = Arc<PoolFailure>;

pub(super) enum Request {
    Acquire {
        reply: oneshot::Sender<Result<PoolLease, PoolError>>,
        cancelled: CancellationToken,
        deadline: Instant,
    },
    Return {
        slot: usize,
        generation: u64,
        reply: Option<Reply>,
    },
    Status(oneshot::Sender<PoolStatus>),
    Shutdown(Reply),
}

struct Waiter {
    reply: oneshot::Sender<Result<PoolLease, PoolError>>,
    cancelled: CancellationToken,
    deadline: Instant,
}

enum State {
    Creating,
    Ready(Arc<Sandbox>),
    Leased {
        sandbox: Arc<Sandbox>,
        expires: Instant,
    },
    Retiring {
        sandbox: Arc<Sandbox>,
        receipt: CleanupReceipt,
    },
    CleanupRequired {
        sandbox: Option<Arc<Sandbox>>,
        receipt: Option<CleanupReceipt>,
        waiting: bool,
        source: Failure,
        retry: Instant,
    },
    Absent,
}

struct Slot {
    generation: u64,
    state: State,
    returns: Vec<Reply>,
}

enum Completion {
    Created {
        slot: usize,
        sandbox: Option<Arc<Sandbox>>,
        result: Result<(), Failure>,
    },
    Destroyed {
        slot: usize,
        result: Result<(), Failure>,
    },
    Receipt {
        slot: usize,
        result: Result<(), Failure>,
    },
}

struct Worker {
    config: PoolConfig,
    metrics: SandboxMetrics,
    slots: Vec<Slot>,
    jobs: FuturesUnordered<BoxFuture<'static, Completion>>,
    waiters: VecDeque<Waiter>,
    shutdowns: Vec<Reply>,
    ready: Option<oneshot::Sender<Result<(), Failure>>>,
    tx: mpsc::WeakUnboundedSender<Request>,
    close: CancellationToken,
    closing: bool,
}

fn failure(operation: &'static str, source: SandboxError) -> Failure {
    Arc::new(PoolFailure::Sandbox { operation, source })
}

pub(super) async fn run(
    config: PoolConfig,
    mut rx: mpsc::UnboundedReceiver<Request>,
    tx: mpsc::UnboundedSender<Request>,
    close: CancellationToken,
    ready: oneshot::Sender<Result<(), Failure>>,
) {
    let metrics = config.metrics.clone();
    let mut worker = Worker {
        slots: (0..config.capacity)
            .map(|_| Slot {
                generation: 0,
                state: State::Absent,
                returns: Vec::new(),
            })
            .collect(),
        config,
        metrics,
        jobs: FuturesUnordered::new(),
        waiters: VecDeque::new(),
        shutdowns: Vec::new(),
        ready: Some(ready),
        tx: tx.downgrade(),
        close,
        closing: false,
    };
    drop(tx);
    for slot in 0..worker.slots.len() {
        worker.create(slot);
    }
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut receiver_closed = false;
    loop {
        if worker.close.is_cancelled() && !worker.closing {
            worker.begin_close();
        }
        worker.maintain();
        if receiver_closed && worker.all_absent() {
            return;
        }
        let next_expiry = worker
            .slots
            .iter()
            .filter_map(|slot| match slot.state {
                State::Leased { expires, .. } => Some(expires),
                _ => None,
            })
            .min();
        let expiry = async {
            match next_expiry {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;
            () = worker.close.cancelled(), if !worker.closing => worker.begin_close(),
            () = expiry => {},
            result = worker.jobs.next(), if !worker.jobs.is_empty() => {
                if let Some(result) = result { worker.complete(result); }
            }
            request = rx.recv(), if !receiver_closed => match request {
                Some(request) => worker.request(request),
                None => { receiver_closed = true; worker.begin_close(); }
            },
            _ = tick.tick() => {}
        }
    }
}

impl Worker {
    fn create(&mut self, slot: usize) {
        self.slots[slot].generation += 1;
        self.slots[slot].state = State::Creating;
        let config = self.config.clone();
        let close = self.close.clone();
        let refill = self.slots[slot].generation > 1;
        if refill {
            self.metrics.record_pool_refilled();
            events::pool_refilled();
        }
        let metrics = self.metrics.clone();
        // Owned by the worker's bounded future set. Never abort a create or
        // discard its eventual ownership receipt when closing/caller cancels.
        self.jobs.push(Box::pin(async move {
            let PoolConfig {
                backend,
                sandbox: sandbox_config,
                baseline,
                ..
            } = config;
            if refill {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let created = Sandbox::create_with_metrics(backend, sandbox_config, metrics).await;
            let sandbox = match created {
                Ok(sandbox) => Arc::new(sandbox),
                Err(source) => {
                    return Completion::Created {
                        slot,
                        sandbox: None,
                        result: Err(failure("create", source)),
                    };
                }
            };
            let result = async {
                if close.is_cancelled() {
                    return Ok(());
                }
                if let Some(baseline) = baseline {
                    sandbox
                        .restore_workspace_snapshot(baseline)
                        .await
                        .map_err(|source| failure("restore workspace", source))?;
                }
                if close.is_cancelled() {
                    return Ok(());
                }
                sandbox
                    .start()
                    .await
                    .map_err(|source| failure("start", source))?;
                if close.is_cancelled() {
                    return Ok(());
                }
                let status = sandbox
                    .status()
                    .await
                    .map_err(|source| failure("status", source))?;
                if status.phase != SandboxPhase::Running {
                    return Err(Arc::new(PoolFailure::NotRunning {
                        phase: status.phase,
                    }));
                }
                Ok(())
            }
            .await;
            Completion::Created {
                slot,
                sandbox: Some(sandbox),
                result,
            }
        }));
    }

    fn retire(&mut self, slot: usize, sandbox: Arc<Sandbox>) {
        self.metrics.record_pool_retired();
        events::pool_retired();
        let receipt = sandbox.revoke();
        self.slots[slot].state = State::Retiring {
            sandbox,
            receipt: receipt.clone(),
        };
        self.jobs.push(Box::pin(async move {
            let result = receipt
                .wait()
                .await
                .map(|_| ())
                .map_err(|source| failure("revoke and destroy", source));
            Completion::Destroyed { slot, result }
        }));
    }

    fn begin_close(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        self.close.cancel();
        while let Some(waiter) = self.waiters.pop_front() {
            let _ = waiter.reply.send(Err(PoolError::Closing));
        }
        for slot in 0..self.slots.len() {
            let sandbox = match &self.slots[slot].state {
                State::Ready(sandbox) | State::Leased { sandbox, .. } => Some(sandbox.clone()),
                _ => None,
            };
            if let Some(sandbox) = sandbox {
                self.retire(slot, sandbox);
            }
        }
    }

    fn complete(&mut self, completion: Completion) {
        match completion {
            Completion::Receipt { slot, result } => match result {
                Ok(()) => {
                    self.slots[slot].state = State::Absent;
                    if !self.closing {
                        self.create(slot);
                    }
                }
                Err(source) => {
                    if let State::CleanupRequired {
                        waiting,
                        retry,
                        source: retained,
                        ..
                    } = &mut self.slots[slot].state
                    {
                        *waiting = false;
                        *retry = Instant::now() + Duration::from_secs(1);
                        *retained = source.clone();
                    }
                    self.fail_shutdowns(source);
                }
            },
            Completion::Created {
                slot,
                sandbox,
                result,
            } => match (sandbox, result) {
                (Some(sandbox), Ok(())) if !self.closing => {
                    self.metrics.record_pool_ready();
                    events::pool_ready();
                    self.slots[slot].state = State::Ready(sandbox);
                }
                (Some(sandbox), result) => {
                    if let Err(source) = result {
                        self.preparation_failed(source);
                    }
                    self.retire(slot, sandbox);
                }
                (None, Err(source)) => {
                    let receipt = match source.as_ref() {
                        PoolFailure::Sandbox {
                            source: SandboxError::CreationCleanupRequired { cleanup, .. },
                            ..
                        } => Some(cleanup.clone()),
                        _ => None,
                    };
                    let uncertain = receipt.is_some()
                        || matches!(
                            source.as_ref(),
                            PoolFailure::Sandbox {
                                source: SandboxError::CleanupRequired { .. }
                                    | SandboxError::WorkerGone { .. },
                                ..
                            }
                        );
                    self.slots[slot].state = if uncertain {
                        State::CleanupRequired {
                            sandbox: None,
                            receipt,
                            waiting: false,
                            source: source.clone(),
                            retry: Instant::now(),
                        }
                    } else {
                        State::Absent
                    };
                    self.preparation_failed(source.clone());
                    if uncertain {
                        self.fail_shutdowns(source);
                    } else if !self.closing {
                        self.create(slot);
                    }
                }
                (None, Ok(())) => unreachable!("successful create has an owner"),
            },
            Completion::Destroyed { slot, result } => match result {
                Ok(()) => {
                    self.slots[slot].state = State::Absent;
                    for reply in self.slots[slot].returns.drain(..) {
                        let _ = reply.send(Ok(()));
                    }
                    if !self.closing {
                        self.create(slot);
                    }
                }
                Err(source) => {
                    let (sandbox, receipt) = match &self.slots[slot].state {
                        State::Retiring { sandbox, receipt } => {
                            (Some(sandbox.clone()), Some(receipt.clone()))
                        }
                        _ => unreachable!("cleanup future owns a retiring slot"),
                    };
                    self.slots[slot].state = State::CleanupRequired {
                        sandbox,
                        receipt,
                        waiting: false,
                        source: source.clone(),
                        retry: Instant::now() + Duration::from_secs(1),
                    };
                    for reply in self.slots[slot].returns.drain(..) {
                        let _ = reply.send(Err(PoolError::Operation {
                            source: source.clone(),
                        }));
                    }
                    self.fail_shutdowns(source);
                }
            },
        }
    }

    fn preparation_failed(&mut self, source: Failure) {
        // Initial warming is all-or-error. Later failures retire only the
        // affected slot, leaving healthy ready slots available during refill.
        if let Some(reply) = self.ready.take() {
            let _ = reply.send(Err(source));
            self.begin_close();
        }
    }

    fn fail_shutdowns(&mut self, source: Failure) {
        for reply in self.shutdowns.drain(..) {
            let _ = reply.send(Err(PoolError::Operation {
                source: source.clone(),
            }));
        }
    }

    fn request(&mut self, request: Request) {
        match request {
            Request::Acquire {
                reply,
                cancelled,
                deadline,
            } => {
                if self.closing || self.close.is_cancelled() {
                    let _ = reply.send(Err(PoolError::Closing));
                } else {
                    self.waiters.push_back(Waiter {
                        reply,
                        cancelled,
                        deadline,
                    });
                }
            }
            Request::Status(reply) => {
                let _ = reply.send(self.status());
            }
            Request::Shutdown(reply) => {
                self.begin_close();
                if let Some(source) = self.slots.iter().find_map(|slot| match &slot.state {
                    State::CleanupRequired { source, .. } => Some(source.clone()),
                    _ => None,
                }) {
                    let _ = reply.send(Err(PoolError::Operation { source }));
                } else {
                    self.shutdowns.push(reply);
                }
            }
            Request::Return {
                slot,
                generation,
                reply,
            } => {
                let Some(owned) = self.slots.get_mut(slot) else {
                    return;
                };
                if owned.generation != generation {
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
                    }
                    return;
                }
                match &owned.state {
                    State::Leased { sandbox, .. } => {
                        events::pool_release();
                        let sandbox = sandbox.clone();
                        if let Some(reply) = reply {
                            owned.returns.push(reply);
                        }
                        self.retire(slot, sandbox);
                    }
                    State::Retiring { .. } => {
                        if let Some(reply) = reply {
                            owned.returns.push(reply);
                        }
                    }
                    State::CleanupRequired { source, .. } => {
                        if let Some(reply) = reply {
                            let _ = reply.send(Err(PoolError::Operation {
                                source: source.clone(),
                            }));
                        }
                    }
                    _ => {
                        if let Some(reply) = reply {
                            let _ = reply.send(Ok(()));
                        }
                    }
                }
            }
        }
    }

    fn maintain(&mut self) {
        let now = Instant::now();
        self.waiters
            .retain(|waiter| !waiter.cancelled.is_cancelled() && !waiter.reply.is_closed());
        let mut live = VecDeque::new();
        while let Some(waiter) = self.waiters.pop_front() {
            if waiter.deadline <= now {
                let _ = waiter.reply.send(Err(PoolError::AcquireTimeout));
            } else {
                live.push_back(waiter);
            }
        }
        self.waiters = live;
        for slot in 0..self.slots.len() {
            let expired = match &self.slots[slot].state {
                State::Leased { sandbox, expires } if *expires <= now => Some(sandbox.clone()),
                _ => None,
            };
            if let Some(sandbox) = expired {
                events::pool_expired();
                self.retire(slot, sandbox);
            }
            let retry = match &self.slots[slot].state {
                State::CleanupRequired {
                    sandbox: Some(sandbox),
                    retry,
                    ..
                } if *retry <= now => Some(sandbox.clone()),
                _ => None,
            };
            if let Some(sandbox) = retry {
                self.retire(slot, sandbox);
            }
            let receipt = match &mut self.slots[slot].state {
                State::CleanupRequired {
                    receipt: Some(receipt),
                    waiting,
                    retry,
                    ..
                } if !*waiting && *retry <= now => {
                    *waiting = true;
                    Some(receipt.clone())
                }
                _ => None,
            };
            if let Some(receipt) = receipt {
                self.jobs.push(Box::pin(async move {
                    let result = receipt
                        .wait()
                        .await
                        .map(|_| ())
                        .map_err(|source| failure("observe owned cleanup", source));
                    Completion::Receipt { slot, result }
                }));
            }
        }
        if !self.closing {
            for slot in 0..self.slots.len() {
                let State::Ready(sandbox) = &self.slots[slot].state else {
                    continue;
                };
                let sandbox = sandbox.clone();
                while let Some(waiter) = self.waiters.pop_front() {
                    if waiter.cancelled.is_cancelled() || waiter.reply.is_closed() {
                        continue;
                    }
                    if waiter.deadline <= Instant::now() {
                        let _ = waiter.reply.send(Err(PoolError::AcquireTimeout));
                        continue;
                    }
                    let Some(tx) = self.tx.upgrade() else {
                        break;
                    };
                    let lease =
                        PoolLease::new(sandbox.clone(), slot, self.slots[slot].generation, tx);
                    // Transfer atomically through oneshot. If receiver vanished,
                    // disarm its return event and keep the never-used slot ready.
                    match waiter.reply.send(Ok(lease)) {
                        Ok(()) => {
                            self.metrics.record_pool_leased();
                            events::pool_handoff();
                            self.slots[slot].state = State::Leased {
                                sandbox: sandbox.clone(),
                                expires: Instant::now() + self.config.lease_ttl,
                            };
                            break;
                        }
                        Err(Ok(mut lease)) => {
                            lease.disarm();
                        }
                        Err(Err(_)) => unreachable!("sent a lease"),
                    }
                }
            }
            if self
                .slots
                .iter()
                .all(|slot| matches!(slot.state, State::Ready(_) | State::Leased { .. }))
                && let Some(reply) = self.ready.take()
            {
                let _ = reply.send(Ok(()));
            }
        }
        if self.closing && self.all_absent() {
            for reply in self.shutdowns.drain(..) {
                let _ = reply.send(Ok(()));
            }
        }
    }

    fn all_absent(&self) -> bool {
        self.slots
            .iter()
            .all(|slot| matches!(slot.state, State::Absent))
    }

    fn status(&self) -> PoolStatus {
        let mut status = PoolStatus {
            capacity: self.slots.len(),
            ready: 0,
            creating: 0,
            leased: 0,
            retiring: 0,
            cleanup_required: 0,
            absent: 0,
            closing: self.closing,
        };
        for slot in &self.slots {
            match &slot.state {
                State::Creating => status.creating += 1,
                State::Ready(_) => status.ready += 1,
                State::Leased { .. } => status.leased += 1,
                State::Retiring { receipt, .. } => {
                    if receipt.last_attempt_failed() {
                        status.cleanup_required += 1;
                    } else {
                        status.retiring += 1;
                    }
                }
                State::CleanupRequired { .. } => status.cleanup_required += 1,
                State::Absent => status.absent += 1,
            }
        }
        status
    }
}
