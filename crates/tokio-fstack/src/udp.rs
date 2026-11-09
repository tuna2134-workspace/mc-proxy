//! Tokio-compatible `UdpSocket` backed by the reactor.

use std::io;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::task::{Context, Poll};

use crate::ops::{self, BufPtr, ConstBufPtr, Op, OpFuture, OpResult};
use crate::poll_io::{self, drive};
use crate::reactor::{self, Io, ReadinessFuture};
use crate::sys;

/// Maximum payload of a single UDP datagram.
const MAX_DATAGRAM: usize = 65535;

struct RecvState {
    op: Option<OpFuture>,
    ready: Option<ReadinessFuture>,
    buf: Vec<u8>,
    /// A received datagram that has not been copied to the caller yet.
    datagram: Option<usize>,
    peer: Option<SocketAddr>,
}

struct SendState {
    op: Option<OpFuture>,
    ready: Option<ReadinessFuture>,
    buf: Vec<u8>,
    /// Number of bytes staged for the in-flight send, if any.
    pending: Option<usize>,
}

/// A UDP socket.
///
/// The API mirrors [`tokio::net::UdpSocket`]: `bind`, `local_addr`,
/// `send_to`/`recv_from`, `send`/`recv` after `connect`, `try_*` variants and
/// `readable`/`writable` readiness helpers.
///
/// # Example
///
/// ```no_run
/// # async fn example() -> std::io::Result<()> {
/// let socket = tokio_fstack::UdpSocket::bind("0.0.0.0:25566").await?;
/// let mut buf = [0u8; 1500];
/// let (n, peer) = socket.recv_from(&mut buf).await?;
/// socket.send_to(&buf[..n], peer).await?;
/// # Ok(())
/// # }
/// ```
pub struct UdpSocket {
    io: Io,
    recv: Mutex<RecvState>,
    send: Mutex<SendState>,
}

impl UdpSocket {
    /// Bind a UDP socket to `addr`.
    pub async fn bind<A: tokio::net::ToSocketAddrs>(addr: A) -> io::Result<UdpSocket> {
        let _ = reactor::reactor()?;
        let addrs = tokio::net::lookup_host(addr).await?;
        let mut last_error = None;
        for addr in addrs {
            match UdpSocket::bind_addr(addr).await {
                Ok(socket) => return Ok(socket),
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address to bind")))
    }

    /// Bind to an already-resolved address.
    pub async fn bind_addr(addr: SocketAddr) -> io::Result<UdpSocket> {
        let _ = reactor::reactor()?;
        let domain = if addr.is_ipv4() {
            libc::AF_INET
        } else {
            libc::AF_INET6
        };
        let fd = match poll_io::submit(Op::Socket {
            domain,
            ty: libc::SOCK_DGRAM,
            proto: 0,
        })
        .await?
        {
            OpResult::Fd(fd) => fd,
            _ => unreachable!("socket returned a non-fd result"),
        };
        let io = Io::new(fd)?;
        poll_io::submit(Op::Bind { fd, addr }).await?;
        Ok(UdpSocket {
            io,
            recv: Mutex::new(RecvState {
                op: None,
                ready: None,
                buf: vec![0u8; MAX_DATAGRAM],
                datagram: None,
                peer: None,
            }),
            send: Mutex::new(SendState {
                op: None,
                ready: None,
                buf: Vec::with_capacity(MAX_DATAGRAM),
                pending: None,
            }),
        })
    }

    /// The local address of the socket.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match ops::submit_blocking(Op::Getsockname { fd: self.io.fd() })? {
            OpResult::Addr(addr) => Ok(addr),
            _ => unreachable!("getsockname returned a non-address result"),
        }
    }

    /// The address `connect` was called with.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        match ops::submit_blocking(Op::Getpeername { fd: self.io.fd() })? {
            OpResult::Addr(addr) => Ok(addr),
            _ => unreachable!("getpeername returned a non-address result"),
        }
    }

    /// Associate the socket with a remote address.
    pub fn connect(&self, addr: SocketAddr) -> io::Result<()> {
        ops::submit_blocking(Op::Connect {
            fd: self.io.fd(),
            addr,
        })?;
        Ok(())
    }

    /// Receive a datagram, returning its payload and source address.
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        std::future::poll_fn(|cx| self.poll_recv_from(cx, buf)).await
    }

    /// Poll for a datagram.
    ///
    /// Cancellation safe: the datagram is read into an internal buffer and
    /// only copied out when the caller polls to completion.
    pub fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr)>> {
        let mut state = self.recv.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut *state;
        let fd = self.io.fd();
        let ptr = BufPtr(state.buf.as_mut_ptr());
        let len = state.buf.len();
        loop {
            if let Some(datagram_len) = state.datagram.take() {
                let n = datagram_len.min(buf.len());
                buf[..n].copy_from_slice(&state.buf[..n]);
                let peer = state.peer.unwrap_or_else(unspecified);
                return Poll::Ready(Ok((n, peer)));
            }
            match drive(
                &mut state.op,
                &mut state.ready,
                &self.io,
                sys::READ,
                cx,
                || Op::RecvFrom { fd, buf: ptr, len },
            ) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(OpResult::Datagram(len, peer))) => {
                    state.datagram = Some(len);
                    state.peer = peer;
                }
                Poll::Ready(Ok(_)) => unreachable!("recvfrom returned an unexpected result"),
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
            }
        }
    }

    /// Send a datagram to `target`.
    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        std::future::poll_fn(|cx| self.poll_send_to(cx, buf, target)).await
    }

    /// Poll a datagram send.
    ///
    /// The payload is copied into an internal staging buffer so that dropping
    /// the future cannot leave the stack thread writing into caller memory.
    pub fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<io::Result<usize>> {
        if buf.len() > MAX_DATAGRAM {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "datagram too large",
            )));
        }
        let mut state = self.send.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut *state;
        if state.pending.is_none() {
            state.buf.clear();
            state.buf.extend_from_slice(buf);
            state.pending = Some(state.buf.len());
        }
        let pending = state.pending.unwrap_or(0);
        let fd = self.io.fd();
        let ptr = ConstBufPtr(state.buf.as_ptr());
        match drive(
            &mut state.op,
            &mut state.ready,
            &self.io,
            sys::WRITE,
            cx,
            || Op::SendTo {
                fd,
                buf: ptr,
                len: pending,
                addr: target,
            },
        ) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(OpResult::Size(n))) => {
                state.pending = None;
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Ok(_)) => unreachable!("sendto returned a non-size result"),
            Poll::Ready(Err(err)) => {
                state.pending = None;
                Poll::Ready(Err(err))
            }
        }
    }

    /// Receive a datagram from the connected peer.
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        std::future::poll_fn(|cx| self.poll_recv(cx, buf)).await
    }

    /// Poll for a datagram from the connected peer.
    pub fn poll_recv(&self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let mut state = self.recv.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut *state;
        let fd = self.io.fd();
        let ptr = BufPtr(state.buf.as_mut_ptr());
        let len = state.buf.len();
        loop {
            if let Some(datagram_len) = state.datagram.take() {
                let n = datagram_len.min(buf.len());
                buf[..n].copy_from_slice(&state.buf[..n]);
                return Poll::Ready(Ok(n));
            }
            match drive(
                &mut state.op,
                &mut state.ready,
                &self.io,
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
                Poll::Ready(Ok(OpResult::Size(len))) => {
                    state.datagram = Some(len);
                }
                Poll::Ready(Ok(_)) => unreachable!("recv returned a non-size result"),
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
            }
        }
    }

    /// Send to the connected peer.
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        std::future::poll_fn(|cx| self.poll_send(cx, buf)).await
    }

    /// Poll a send to the connected peer.
    pub fn poll_send(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        if buf.len() > MAX_DATAGRAM {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "datagram too large",
            )));
        }
        let mut state = self.send.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut *state;
        if state.pending.is_none() {
            state.buf.clear();
            state.buf.extend_from_slice(buf);
            state.pending = Some(state.buf.len());
        }
        let pending = state.pending.unwrap_or(0);
        let fd = self.io.fd();
        let ptr = ConstBufPtr(state.buf.as_ptr());
        match drive(
            &mut state.op,
            &mut state.ready,
            &self.io,
            sys::WRITE,
            cx,
            || Op::Send {
                fd,
                buf: ptr,
                len: pending,
            },
        ) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(OpResult::Size(n))) => {
                state.pending = None;
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Ok(_)) => unreachable!("send returned a non-size result"),
            Poll::Ready(Err(err)) => {
                state.pending = None;
                Poll::Ready(Err(err))
            }
        }
    }

    /// Try to send without waiting.
    pub fn try_send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        match ops::submit_blocking(Op::SendTo {
            fd: self.io.fd(),
            buf: ConstBufPtr(buf.as_ptr()),
            len: buf.len(),
            addr: target,
        })? {
            OpResult::Size(n) => Ok(n),
            _ => unreachable!("sendto returned a non-size result"),
        }
    }

    /// Try to receive without waiting.
    pub fn try_recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match ops::submit_blocking(Op::RecvFrom {
            fd: self.io.fd(),
            buf: BufPtr(buf.as_mut_ptr()),
            len: buf.len(),
        })? {
            OpResult::Datagram(n, peer) => Ok((n, peer.unwrap_or_else(unspecified))),
            _ => unreachable!("recvfrom returned an unexpected result"),
        }
    }

    /// Wait until the socket is readable.
    pub async fn readable(&self) -> io::Result<()> {
        self.io.readable().await;
        Ok(())
    }

    /// Wait until the socket is writable.
    pub async fn writable(&self) -> io::Result<()> {
        self.io.writable().await;
        Ok(())
    }
}

impl std::fmt::Debug for UdpSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UdpSocket")
            .field("fd", &self.io.fd())
            .finish()
    }
}

fn unspecified() -> SocketAddr {
    use std::net::{Ipv4Addr, SocketAddrV4};
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
}
