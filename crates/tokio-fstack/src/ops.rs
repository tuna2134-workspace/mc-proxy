//! Asynchronous submission of non-blocking syscalls.
//!
//! Every socket operation is turned into an [`Op`] and handed to the active
//! [`crate::reactor::Platform`]. The standard platform executes it inline on
//! the calling thread; the F-Stack platform forwards it to the single thread
//! that owns the F-Stack instance (see `reactor.rs` for why that is required).
//!
//! # Buffer lifetime and cancellation
//!
//! Read/write operations execute straight into caller-provided buffers, which
//! is what makes the hot path allocation-free. That is only sound because of
//! the locking protocol implemented here: the platform executes the syscall
//! while holding the completion slot's mutex, and [`OpFuture::drop`] acquires
//! the same mutex. Dropping an in-flight future therefore waits until the
//! syscall has finished, so the buffer can never be released while the stack
//! thread is still using it.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};

use crate::sys;

/// A mutable buffer pointer that is safe to move to the F-Stack thread.
#[derive(Clone, Copy)]
pub(crate) struct BufPtr(pub(crate) *mut u8);

// SAFETY: the pointer is only dereferenced by the platform while executing the
// operation, and `OpFuture`'s drop protocol guarantees the pointed-to buffer
// outlives that execution.
unsafe impl Send for BufPtr {}

/// A shared buffer pointer that is safe to move to the F-Stack thread.
#[derive(Clone, Copy)]
pub(crate) struct ConstBufPtr(pub(crate) *const u8);

// SAFETY: see `BufPtr`.
unsafe impl Send for ConstBufPtr {}

/// One non-blocking syscall.
pub(crate) enum Op {
    Socket {
        domain: i32,
        ty: i32,
        proto: i32,
    },
    Bind {
        fd: i32,
        addr: SocketAddr,
    },
    Listen {
        fd: i32,
        backlog: i32,
    },
    Accept {
        fd: i32,
    },
    Connect {
        fd: i32,
        addr: SocketAddr,
    },
    Getsockname {
        fd: i32,
    },
    Getpeername {
        fd: i32,
    },
    SetSockOpt {
        fd: i32,
        level: i32,
        name: i32,
        value: i32,
    },
    GetSockOpt {
        fd: i32,
        level: i32,
        name: i32,
    },
    Recv {
        fd: i32,
        buf: BufPtr,
        len: usize,
        flags: i32,
    },
    Send {
        fd: i32,
        buf: ConstBufPtr,
        len: usize,
    },
    RecvFrom {
        fd: i32,
        buf: BufPtr,
        len: usize,
    },
    SendTo {
        fd: i32,
        buf: ConstBufPtr,
        len: usize,
        addr: SocketAddr,
    },
}

/// The result of an [`Op`].
#[derive(Debug)]
pub(crate) enum OpResult {
    Fd(i32),
    Accepted(i32, SocketAddr),
    Size(usize),
    Addr(SocketAddr),
    Int(i32),
    Datagram(usize, Option<SocketAddr>),
    Unit,
}

/// The syscall identifier used for diagnostics (unused when tracing is off).
#[allow(dead_code)]
fn op_name(op: &Op) -> &'static str {
    match op {
        Op::Socket { .. } => "socket",
        Op::Bind { .. } => "bind",
        Op::Listen { .. } => "listen",
        Op::Accept { .. } => "accept",
        Op::Connect { .. } => "connect",
        Op::Getsockname { .. } => "getsockname",
        Op::Getpeername { .. } => "getpeername",
        Op::SetSockOpt { .. } => "setsockopt",
        Op::GetSockOpt { .. } => "getsockopt",
        Op::Recv { .. } => "recv",
        Op::Send { .. } => "send",
        Op::RecvFrom { .. } => "recvfrom",
        Op::SendTo { .. } => "sendto",
    }
}

impl Op {
    /// Execute the syscall. Must only be called on the thread that owns the
    /// socket (the F-Stack thread for F-Stack).
    pub(crate) fn execute(self) -> io::Result<OpResult> {
        match self {
            Op::Socket { domain, ty, proto } => sys::socket(domain, ty, proto).map(OpResult::Fd),
            Op::Bind { fd, addr } => sys::bind(fd, addr).map(|_| OpResult::Unit),
            Op::Listen { fd, backlog } => sys::listen(fd, backlog).map(|_| OpResult::Unit),
            Op::Accept { fd } => sys::accept(fd).map(|(fd, peer)| OpResult::Accepted(fd, peer)),
            Op::Connect { fd, addr } => sys::connect(fd, addr).map(|_| OpResult::Unit),
            Op::Getsockname { fd } => sys::getsockname(fd).map(OpResult::Addr),
            Op::Getpeername { fd } => sys::getpeername(fd).map(OpResult::Addr),
            Op::SetSockOpt {
                fd,
                level,
                name,
                value,
            } => sys::setsockopt_int(fd, level, name, value).map(|_| OpResult::Unit),
            Op::GetSockOpt { fd, level, name } => {
                sys::getsockopt_int(fd, level, name).map(OpResult::Int)
            }
            Op::Recv {
                fd,
                buf,
                len,
                flags,
            } => sys::recv(fd, buf.0, len, flags).map(OpResult::Size),
            Op::Send { fd, buf, len } => sys::send(fd, buf.0, len).map(OpResult::Size),
            Op::RecvFrom { fd, buf, len } => {
                sys::recv_from(fd, buf.0, len).map(|(n, addr)| OpResult::Datagram(n, addr))
            }
            Op::SendTo { fd, buf, len, addr } => {
                sys::send_to(fd, buf.0, len, addr).map(OpResult::Size)
            }
        }
    }
}

struct SlotState {
    result: Option<io::Result<OpResult>>,
    waker: Option<Waker>,
    cancelled: bool,
}

/// Completion slot shared between a future and the thread that runs the op.
pub(crate) struct Slot {
    state: Mutex<SlotState>,
    notify: Condvar,
    id: u64,
}

impl Slot {
    fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            state: Mutex::new(SlotState {
                result: None,
                waker: None,
                cancelled: false,
            }),
            notify: Condvar::new(),
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Execute `op` and publish its result.
    ///
    /// The syscall runs while the slot mutex is held: this is what makes
    /// dropping an in-flight [`OpFuture`] safe (see the module docs).
    pub(crate) fn run(&self, op: Op) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.cancelled {
            return;
        }
        let result = op.execute();
        state.result = Some(result);
        let waker = state.waker.take();
        drop(state);
        self.notify.notify_all();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Return the slot id, for debugging and metrics.
    #[allow(dead_code)]
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
}

/// A pending syscall operation.
///
/// Implements [`Future`] as well as an inherent [`OpFuture::poll`] so that it
/// can be driven both by `async` code and from inside `poll_read`-style trait
/// implementations without `Pin` ceremony.
pub(crate) struct OpFuture {
    slot: Arc<Slot>,
}

impl OpFuture {
    fn new(slot: Arc<Slot>) -> Self {
        Self { slot }
    }

    /// Poll the completion slot.
    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<OpResult>> {
        let mut state = self.slot.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(result) = state.result.take() {
            return Poll::Ready(result);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Future for OpFuture {
    type Output = io::Result<OpResult>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `OpFuture` is `Unpin`: it only owns an `Arc`.
        let this = self.get_mut();
        OpFuture::poll(this, cx)
    }
}

impl Drop for OpFuture {
    fn drop(&mut self) {
        // Blocking here is intentional and bounded: the platform holds this
        // mutex only for the duration of one non-blocking syscall.
        let mut state = self.slot.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.result.is_none() {
            state.cancelled = true;
        }
    }
}

/// Submit an operation to the active platform.
pub(crate) fn submit(op: Op) -> OpFuture {
    let slot = Arc::new(Slot::new());
    crate::reactor::dispatch(crate::reactor::Command::Op(op, slot.clone()));
    OpFuture::new(slot)
}

/// Submit an operation and block the calling thread until it has completed.
///
/// Used by the synchronous parts of the API (`local_addr`, `set_nodelay`,
/// `try_send_to`, ...). Must not be called from the reactor thread; that would
/// dead-lock, and no public type exposes the reactor thread handle.
pub(crate) fn submit_blocking(op: Op) -> io::Result<OpResult> {
    let slot = Arc::new(Slot::new());
    crate::reactor::dispatch(crate::reactor::Command::Op(op, slot.clone()));
    let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(result) = state.result.take() {
            return result;
        }
        state = slot.notify.wait(state).unwrap_or_else(|e| e.into_inner());
    }
}
