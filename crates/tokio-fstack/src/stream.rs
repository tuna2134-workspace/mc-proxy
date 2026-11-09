//! Tokio-compatible `TcpStream` backed by the reactor.

use std::io;
use std::net::{Shutdown, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::ops::{self, BufPtr, ConstBufPtr, Op, OpFuture, OpResult};
use crate::poll_io::{self, drive};
use crate::reactor::{self, Io, ReadinessFuture};
use crate::sys;

/// Initial size of a stream's internal read buffer.
const INITIAL_READ_BUFFER: usize = 8 * 1024;
/// Upper bound for the internal read buffer (grows when reads fill it).
const MAX_READ_BUFFER: usize = 64 * 1024;
/// Maximum number of bytes copied into the write staging buffer per call.
const MAX_WRITE_CHUNK: usize = 32 * 1024;

/// A connected TCP stream.
///
/// The API mirrors [`tokio::net::TcpStream`]: `connect`, `local_addr`,
/// `peer_addr`, `set_nodelay`, `shutdown`, `into_split`, plus implementations
/// of [`AsyncRead`]/[`AsyncWrite`]. It can therefore be used with
/// `AsyncReadExt`/`AsyncWriteExt`, `tokio::io::copy`, `tokio::select!`, etc.
///
/// # Example
///
/// ```no_run
/// # async fn example() -> std::io::Result<()> {
/// use tokio::io::{AsyncReadExt, AsyncWriteExt};
///
/// let mut stream = tokio_fstack::TcpStream::connect("127.0.0.1:25565").await?;
/// stream.write_all(b"hello").await?;
/// let mut buf = [0u8; 16];
/// let n = stream.read(&mut buf).await?;
/// println!("read {n} bytes: {:?}", &buf[..n]);
/// # Ok(())
/// # }
/// ```
pub struct TcpStream {
    inner: Arc<StreamInner>,
}

pub(crate) struct StreamInner {
    pub(crate) io: Io,
    read: Mutex<ReadState>,
    write: Mutex<WriteState>,
}

struct ReadState {
    op: Option<OpFuture>,
    ready: Option<ReadinessFuture>,
    buf: Vec<u8>,
    /// Number of buffered bytes available at `buf[pos..pos + pending]`.
    pending: usize,
    pos: usize,
}

impl ReadState {
    fn new() -> Self {
        Self {
            op: None,
            ready: None,
            buf: vec![0u8; INITIAL_READ_BUFFER],
            pending: 0,
            pos: 0,
        }
    }
}

struct WriteState {
    op: Option<OpFuture>,
    ready: Option<ReadinessFuture>,
    buf: Vec<u8>,
    /// Number of bytes in `buf` that still have to be handed to the socket.
    pending: usize,
}

impl WriteState {
    fn new() -> Self {
        Self {
            op: None,
            ready: None,
            buf: Vec::with_capacity(MAX_WRITE_CHUNK),
            pending: 0,
        }
    }
}

impl TcpStream {
    /// Open a TCP connection to `addr`.
    ///
    /// Name resolution uses [`tokio::net::lookup_host`], so it must be called
    /// from within a Tokio runtime. On an F-Stack-only host, prefer literal IP
    /// addresses: the kernel resolver is not reachable once DPDK owns the NIC.
    pub async fn connect<A: tokio::net::ToSocketAddrs>(addr: A) -> io::Result<TcpStream> {
        let addrs = tokio::net::lookup_host(addr).await?;
        let mut last_error = None;
        for addr in addrs {
            match TcpStream::connect_addr(addr).await {
                Ok(stream) => return Ok(stream),
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "could not resolve address")
        }))
    }

    /// Connect to an already-resolved address.
    pub async fn connect_addr(addr: SocketAddr) -> io::Result<TcpStream> {
        let _ = reactor::reactor()?;
        let domain = if addr.is_ipv4() {
            libc::AF_INET
        } else {
            libc::AF_INET6
        };
        let fd = match poll_io::submit(Op::Socket {
            domain,
            ty: libc::SOCK_STREAM,
            proto: 0,
        })
        .await?
        {
            OpResult::Fd(fd) => fd,
            _ => unreachable!("socket returned a non-fd result"),
        };
        let io = match Io::new(fd) {
            Ok(io) => io,
            Err(err) => {
                reactor::close_raw(fd);
                return Err(err);
            }
        };
        let connect = poll_io::submit(Op::Connect { fd, addr }).await;
        match connect {
            Ok(_) => {}
            Err(err) if sys::is_in_progress(&err) => {
                io.writable().await;
                match poll_io::submit(Op::GetSockOpt {
                    fd,
                    level: libc::SOL_SOCKET,
                    name: libc::SO_ERROR,
                })
                .await?
                {
                    OpResult::Int(0) => {}
                    OpResult::Int(code) => {
                        return Err(io::Error::from_raw_os_error(code));
                    }
                    _ => unreachable!("getsockopt returned a non-int result"),
                }
            }
            Err(err) => return Err(err),
        }
        Ok(TcpStream {
            inner: Arc::new(StreamInner {
                io,
                read: Mutex::new(ReadState::new()),
                write: Mutex::new(WriteState::new()),
            }),
        })
    }

    pub(crate) fn from_io(io: Io) -> TcpStream {
        TcpStream {
            inner: Arc::new(StreamInner {
                io,
                read: Mutex::new(ReadState::new()),
                write: Mutex::new(WriteState::new()),
            }),
        }
    }

    /// The local address of this stream.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match ops::submit_blocking(Op::Getsockname {
            fd: self.inner.io.fd(),
        })? {
            OpResult::Addr(addr) => Ok(addr),
            _ => unreachable!("getsockname returned a non-address result"),
        }
    }

    /// The peer address of this stream.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        match ops::submit_blocking(Op::Getpeername {
            fd: self.inner.io.fd(),
        })? {
            OpResult::Addr(addr) => Ok(addr),
            _ => unreachable!("getpeername returned a non-address result"),
        }
    }

    /// Enable or disable Nagle's algorithm.
    ///
    /// On the F-Stack backend the option is applied on the F-Stack thread, so
    /// the call blocks until it has been executed.
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        ops::submit_blocking(Op::SetSockOpt {
            fd: self.inner.io.fd(),
            level: libc::IPPROTO_TCP,
            name: libc::TCP_NODELAY,
            value: i32::from(nodelay),
        })?;
        Ok(())
    }

    /// Whether Nagle's algorithm is disabled.
    pub fn nodelay(&self) -> io::Result<bool> {
        match ops::submit_blocking(Op::GetSockOpt {
            fd: self.inner.io.fd(),
            level: libc::IPPROTO_TCP,
            name: libc::TCP_NODELAY,
        })? {
            OpResult::Int(value) => Ok(value != 0),
            _ => unreachable!("getsockopt returned a non-int result"),
        }
    }

    /// Shut down part of the connection.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        let how = match how {
            Shutdown::Read => libc::SHUT_RD,
            Shutdown::Write => libc::SHUT_WR,
            Shutdown::Both => libc::SHUT_RDWR,
        };
        reactor::shutdown(self.inner.io.fd(), how);
        Ok(())
    }

    /// Split the stream into owned read and write halves.
    pub fn into_split(self) -> (OwnedReadHalf, OwnedWriteHalf) {
        (
            OwnedReadHalf {
                inner: Arc::clone(&self.inner),
            },
            OwnedWriteHalf {
                inner: self.inner,
                shutdown_on_drop: true,
            },
        )
    }

    /// Peek at incoming data without consuming it.
    pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut op = None;
        let mut ready = None;
        let io = &self.inner.io;
        let fd = io.fd();
        let ptr = BufPtr(buf.as_mut_ptr());
        let len = buf.len();
        std::future::poll_fn(|cx| {
            match drive(&mut op, &mut ready, io, sys::READ, cx, || Op::Recv {
                fd,
                buf: ptr,
                len,
                flags: libc::MSG_PEEK,
            }) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(OpResult::Size(n))) => Poll::Ready(Ok(n)),
                Poll::Ready(Ok(_)) => unreachable!("recv returned a non-size result"),
                Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            }
        })
        .await
    }

    /// Returns `true` if the socket is known to be readable.
    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.inner.read.lock() {
            Ok(state) if state.pending > 0 => Poll::Ready(Ok(())),
            _ => match self.inner.io.readable().poll(cx) {
                Poll::Ready(()) => Poll::Ready(Ok(())),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

fn poll_read_inner(
    inner: &StreamInner,
    cx: &mut Context<'_>,
    buf: &mut ReadBuf<'_>,
) -> Poll<io::Result<()>> {
    if buf.remaining() == 0 {
        return Poll::Ready(Ok(()));
    }
    let mut state = inner.read.lock().unwrap_or_else(|e| e.into_inner());
    let state = &mut *state;
    loop {
        if state.pending > 0 {
            let n = state.pending.min(buf.remaining());
            buf.put_slice(&state.buf[state.pos..state.pos + n]);
            state.pos += n;
            state.pending -= n;
            if state.pending == 0 {
                state.pos = 0;
            }
            return Poll::Ready(Ok(()));
        }
        let fd = inner.io.fd();
        let ptr = BufPtr(state.buf.as_mut_ptr());
        let len = state.buf.len();
        match drive(
            &mut state.op,
            &mut state.ready,
            &inner.io,
            sys::READ,
            cx,
            || Op::Recv {
                fd,
                buf: ptr,
                len,
                flags: 0,
            },
        ) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Ok(OpResult::Size(0))) => return Poll::Ready(Ok(())),
            Poll::Ready(Ok(OpResult::Size(n))) => {
                state.pending = n;
                state.pos = 0;
                if n == state.buf.len() && state.buf.len() < MAX_READ_BUFFER {
                    let next = (state.buf.len() * 2).min(MAX_READ_BUFFER);
                    state.buf.resize(next, 0);
                }
            }
            Poll::Ready(Ok(_)) => unreachable!("recv returned a non-size result"),
            Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
        }
    }
}

fn poll_write_inner(
    inner: &StreamInner,
    cx: &mut Context<'_>,
    src: &[u8],
) -> Poll<io::Result<usize>> {
    if src.is_empty() {
        return Poll::Ready(Ok(0));
    }
    let mut state = inner.write.lock().unwrap_or_else(|e| e.into_inner());
    let state = &mut *state;
    if state.pending == 0 {
        let n = src.len().min(MAX_WRITE_CHUNK);
        state.buf.clear();
        state.buf.extend_from_slice(&src[..n]);
        state.pending = n;
    }
    let fd = inner.io.fd();
    let ptr = ConstBufPtr(state.buf.as_ptr());
    let len = state.pending;
    match drive(
        &mut state.op,
        &mut state.ready,
        &inner.io,
        sys::WRITE,
        cx,
        || Op::Send { fd, buf: ptr, len },
    ) {
        Poll::Pending => Poll::Pending,
        Poll::Ready(Ok(OpResult::Size(n))) => {
            state.pending = 0;
            Poll::Ready(Ok(n))
        }
        Poll::Ready(Ok(_)) => unreachable!("send returned a non-size result"),
        Poll::Ready(Err(err)) => {
            state.pending = 0;
            Poll::Ready(Err(err))
        }
    }
}

impl AsyncRead for TcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        poll_read_inner(&self.inner, cx, buf)
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        poll_write_inner(&self.inner, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Packets are handed to the F-Stack thread immediately; there is no
        // user-space write buffer to flush beyond the in-flight op.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        reactor::shutdown(self.inner.io.fd(), libc::SHUT_WR);
        Poll::Ready(Ok(()))
    }
}

impl std::fmt::Debug for TcpStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpStream")
            .field("fd", &self.inner.io.fd())
            .finish()
    }
}

/// Owned read half returned by [`TcpStream::into_split`].
pub struct OwnedReadHalf {
    inner: Arc<StreamInner>,
}

/// Owned write half returned by [`TcpStream::into_split`].
pub struct OwnedWriteHalf {
    inner: Arc<StreamInner>,
    shutdown_on_drop: bool,
}

impl OwnedReadHalf {
    /// The local address of the underlying stream.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match ops::submit_blocking(Op::Getsockname {
            fd: self.inner.io.fd(),
        })? {
            OpResult::Addr(addr) => Ok(addr),
            _ => unreachable!(),
        }
    }

    /// The peer address of the underlying stream.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        match ops::submit_blocking(Op::Getpeername {
            fd: self.inner.io.fd(),
        })? {
            OpResult::Addr(addr) => Ok(addr),
            _ => unreachable!(),
        }
    }
}

impl OwnedWriteHalf {
    /// Do not shut the write side down when this half is dropped.
    pub fn forget(mut self) {
        self.shutdown_on_drop = false;
    }
}

impl Drop for OwnedWriteHalf {
    fn drop(&mut self) {
        if self.shutdown_on_drop {
            reactor::shutdown(self.inner.io.fd(), libc::SHUT_WR);
        }
    }
}

impl AsyncRead for OwnedReadHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        poll_read_inner(&self.inner, cx, buf)
    }
}

impl AsyncWrite for OwnedReadHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        poll_write_inner(&self.inner, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        reactor::shutdown(self.inner.io.fd(), libc::SHUT_WR);
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for OwnedWriteHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        poll_read_inner(&self.inner, cx, buf)
    }
}

impl AsyncWrite for OwnedWriteHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        poll_write_inner(&self.inner, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        reactor::shutdown(self.inner.io.fd(), libc::SHUT_WR);
        Poll::Ready(Ok(()))
    }
}
