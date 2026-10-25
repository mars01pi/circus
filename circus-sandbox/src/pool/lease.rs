use std::fmt;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use super::{PoolError, worker::Request};
use crate::Sandbox;

/// Exclusive, single-use access to one prestarted sandbox.
///
/// This handle cannot be cloned, and only a borrowed sandbox is exposed.
/// Its TTL starts at handoff and is never extended by activity. Expiry or
/// `shutdown` revokes even in-flight commands. Dropping requests destruction,
/// not reset/reuse, and does not prove success.
/// The pool worker retains independent ownership until absence is confirmed.
pub struct PoolLease {
    sandbox: Arc<Sandbox>,
    slot: usize,
    generation: u64,
    tx: mpsc::UnboundedSender<Request>,
    returned: bool,
}

impl fmt::Debug for PoolLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolLease")
            .field("slot", &self.slot)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl PoolLease {
    pub(super) fn new(
        sandbox: Arc<Sandbox>,
        slot: usize,
        generation: u64,
        tx: mpsc::UnboundedSender<Request>,
    ) -> Self {
        Self {
            sandbox,
            slot,
            generation,
            tx,
            returned: false,
        }
    }

    pub(super) fn disarm(&mut self) {
        self.returned = true;
    }

    /// Borrows this lease's sandbox; no cloneable/escaping ownership is exposed.
    /// Pool shutdown revokes the underlying container even during this borrow.
    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    /// Destroys this container. A successful result confirms absence, not a
    /// reusable clean container. Failed cleanup remains owned and is retried.
    /// Cancelling this future still leaves the worker responsible for cleanup.
    pub async fn release(mut self) -> Result<(), PoolError> {
        let (reply, result) = oneshot::channel();
        self.tx
            .send(Request::Return {
                slot: self.slot,
                generation: self.generation,
                reply: Some(reply),
            })
            .map_err(|_| PoolError::WorkerGone)?;
        self.returned = true;
        result.await.map_err(|_| PoolError::WorkerGone)?
    }
}

impl Drop for PoolLease {
    fn drop(&mut self) {
        if !self.returned {
            let _ = self.tx.send(Request::Return {
                slot: self.slot,
                generation: self.generation,
                reply: None,
            });
        }
    }
}
