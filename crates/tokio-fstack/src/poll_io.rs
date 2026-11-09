//! Shared machinery for driving an asynchronous operation to completion.
//!
//! An operation is retried whenever the socket reports `EAGAIN`: the driver
//! arms readiness for the direction that made progress impossible and
//! re-submits the exact same operation once the reactor wakes the task.

use std::task::{Context, Poll};

use crate::ops::{self, Op, OpFuture, OpResult};
use crate::reactor::{Io, ReadinessFuture};
use crate::sys;

/// Drive `make()` until it completes without `EAGAIN`.
///
/// `op` and `ready` are the caller's in-flight slots; they must live as long
/// as the poll loop so that cancellations are handled by [`OpFuture`]'s drop
/// protocol.
pub(crate) fn drive(
    op: &mut Option<OpFuture>,
    ready: &mut Option<ReadinessFuture>,
    io: &Io,
    direction: u8,
    cx: &mut Context<'_>,
    mut make: impl FnMut() -> Op,
) -> Poll<std::io::Result<OpResult>> {
    loop {
        if let Some(mut future) = op.take() {
            match future.poll(cx) {
                Poll::Pending => {
                    *op = Some(future);
                    return Poll::Pending;
                }
                Poll::Ready(Err(err)) if sys::is_would_block(&err) => {
                    *ready = Some(if direction & sys::READ != 0 {
                        io.readable()
                    } else {
                        io.writable()
                    });
                }
                Poll::Ready(result) => return Poll::Ready(result),
            }
            continue;
        }
        if let Some(mut readiness) = ready.take() {
            match readiness.poll(cx) {
                Poll::Pending => {
                    *ready = Some(readiness);
                    return Poll::Pending;
                }
                Poll::Ready(()) => {}
            }
            continue;
        }
        *op = Some(ops::submit(make()));
    }
}

/// Submit an operation and await its result.
pub(crate) async fn submit(op: Op) -> std::io::Result<OpResult> {
    ops::submit(op).await
}
