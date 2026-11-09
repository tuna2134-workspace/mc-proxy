//! Tokio-compatible `TcpListener` backed by the reactor.

use std::io;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::task::{Context, Poll};

use crate::ops::{Op, OpFuture, OpResult};
use crate::poll_io::{self, drive};
use crate::reactor::{self, Io, ReadinessFuture};
use crate::stream::TcpStream;
use crate::sys;

struct AcceptState {
    op: Option<OpFuture>,
    ready: Option<ReadinessFuture>,
}

/// A TCP socket server.
///
/// The API mirrors [`tokio::net::TcpListener`]: `bind`, `local_addr`, `accept`,
/// `poll_accept` and a pull-based [`TcpListener::incoming`] helper.
///
/// # Example
///
/// ```no_run
/// # async fn example() -> std::io::Result<()> {
/// let listener = tokio_fstack::TcpListener::bind("0.0.0.0:25565").await?;
/// loop {
///     let (stream, peer) = listener.accept().await?;
///     println!("connection from {peer}");
///     drop(stream);
/// }
/// # }
/// ```
pub struct TcpListener {
    io: Io,
    accept: Mutex<AcceptState>,
}

impl TcpListener {
    /// Bind a TCP listener to `addr`.
    pub async fn bind<A: tokio::net::ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        Self::bind_inner(addr, false).await
    }

    /// Bind a TCP listener with `SO_REUSEPORT`.
    ///
    /// Several workers can then bind the same address; the kernel/F-Stack
    /// distributes incoming connections between them.
    pub async fn bind_reuseport<A: tokio::net::ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        Self::bind_inner(addr, true).await
    }

    async fn bind_inner<A: tokio::net::ToSocketAddrs>(
        addr: A,
        reuseport: bool,
    ) -> io::Result<TcpListener> {
        let _ = reactor::reactor()?;
        let addrs = tokio::net::lookup_host(addr).await?;
        let mut last_error = None;
        for addr in addrs {
            match TcpListener::bind_addr_inner(addr, reuseport).await {
                Ok(listener) => return Ok(listener),
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address to bind")))
    }

    /// Bind to an already-resolved address.
    pub async fn bind_addr(addr: SocketAddr) -> io::Result<TcpListener> {
        Self::bind_addr_inner(addr, false).await
    }

    /// Bind to an already-resolved address with `SO_REUSEPORT`.
    pub async fn bind_reuseport_addr(addr: SocketAddr) -> io::Result<TcpListener> {
        Self::bind_addr_inner(addr, true).await
    }

    async fn bind_addr_inner(addr: SocketAddr, reuseport: bool) -> io::Result<TcpListener> {
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
        let io = Io::new(fd)?;
        poll_io::submit(Op::SetSockOpt {
            fd,
            level: libc::SOL_SOCKET,
            name: libc::SO_REUSEADDR,
            value: 1,
        })
        .await?;
        if reuseport {
            poll_io::submit(Op::SetSockOpt {
                fd,
                level: libc::SOL_SOCKET,
                name: libc::SO_REUSEPORT,
                value: 1,
            })
            .await?;
        }
        poll_io::submit(Op::Bind { fd, addr }).await?;
        poll_io::submit(Op::Listen { fd, backlog: 1024 }).await?;
        Ok(TcpListener {
            io,
            accept: Mutex::new(AcceptState {
                op: None,
                ready: None,
            }),
        })
    }

    /// The local address the listener is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match crate::ops::submit_blocking(Op::Getsockname { fd: self.io.fd() })? {
            OpResult::Addr(addr) => Ok(addr),
            _ => unreachable!("getsockname returned a non-address result"),
        }
    }

    /// Poll for an incoming connection.
    ///
    /// This is cancellation safe: dropping the returned future does not lose
    /// an accepted socket.
    pub fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<io::Result<(TcpStream, SocketAddr)>> {
        let mut state = self.accept.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut *state;
        let fd = self.io.fd();
        match drive(
            &mut state.op,
            &mut state.ready,
            &self.io,
            sys::READ,
            cx,
            || Op::Accept { fd },
        ) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(OpResult::Accepted(fd, peer))) => {
                let stream = match Io::new(fd) {
                    Ok(io) => TcpStream::from_io(io),
                    Err(err) => return Poll::Ready(Err(err)),
                };
                Poll::Ready(Ok((stream, peer)))
            }
            Poll::Ready(Ok(_)) => unreachable!("accept returned an unexpected result"),
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
        }
    }

    /// Accept a new connection.
    pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        std::future::poll_fn(|cx| self.poll_accept(cx)).await
    }

    /// Pull-based iterator over incoming connections.
    pub fn incoming(&self) -> Incoming<'_> {
        Incoming { listener: self }
    }
}

impl std::fmt::Debug for TcpListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpListener")
            .field("fd", &self.io.fd())
            .finish()
    }
}

/// Iterator over connections accepted by a [`TcpListener`].
///
/// Unlike `tokio::net::TcpListener::incoming`, this is a plain async `next`
/// loop so that `tokio-stream` is not required.
pub struct Incoming<'a> {
    listener: &'a TcpListener,
}

impl Incoming<'_> {
    /// Wait for the next connection.
    pub async fn next(&mut self) -> io::Result<(TcpStream, SocketAddr)> {
        self.listener.accept().await
    }
}
