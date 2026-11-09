//! Tokio-compatible TCP/UDP sockets for [F-Stack](https://f-stack.org/) (DPDK)
//! and plain Linux.
//!
//! This crate provides drop-in replacements for the most commonly used parts
//! of [`tokio::net`]:
//!
//! | this crate | Tokio equivalent |
//! |---|---|
//! | [`TcpListener`] | [`tokio::net::TcpListener`] |
//! | [`TcpStream`] | [`tokio::net::TcpStream`] |
//! | [`OwnedReadHalf`] / [`OwnedWriteHalf`] | same |
//! | [`UdpSocket`] | [`tokio::net::UdpSocket`] |
//!
//! All types are usable from ordinary Tokio tasks (`tokio::spawn`,
//! `tokio::select!`, `AsyncReadExt`, `AsyncWriteExt`, ...). The difference is
//! where the sockets live: instead of the host kernel they can live in
//! F-Stack's user-space TCP/IP stack (DPDK).
//!
//! # Example
//!
//! ```no_run
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//!
//! # async fn example() -> std::io::Result<()> {
//! let listener = tokio_fstack::TcpListener::bind("0.0.0.0:25565").await?;
//! loop {
//!     let (mut stream, peer) = listener.accept().await?;
//!     tokio::spawn(async move {
//!         let mut buf = [0u8; 1024];
//!         let n = stream.read(&mut buf).await.unwrap_or(0);
//!         let _ = stream.write_all(&buf[..n]).await;
//!         let _ = peer;
//!     });
//! }
//! # }
//! ```
//!
//! # Two backends
//!
//! * **standard** (default): the sockets are ordinary kernel sockets. This
//!   backend exists so that programs (and the test-suite) can exercise exactly
//!   the same async code paths without DPDK hardware.
//! * **`fstack`**: sockets are created with `ff_socket` and driven by
//!   `ff_epoll_*`. Enable it with `--features fstack` and link `libfstack`.
//!
//! ## F-Stack threading model
//!
//! F-Stack is a single-threaded network stack: only the thread that called
//! `ff_init`/`ff_run` may call `ff_*` functions, and those calls must happen
//! on the process main thread. Tokio, on the other hand, runs tasks on a
//! thread pool. This crate bridges the two models with [`run_with_stack`]:
//! `ff_init` runs on the main thread, the application (Tokio runtime) runs on
//! a dedicated thread, and *every* socket syscall and epoll update is queued
//! back to the main thread's `ff_run` loop (see `reactor.rs`). No F-Stack
//! function is ever called from a Tokio worker thread.
//!
//! The trade-off is one cross-thread hand-off per I/O operation. For a
//! connection-oriented proxy this is far below the packet-processing cost of
//! the Minecraft protocol, and it makes the integration sound by construction.
//!
//! ## Initialisation
//!
//! Call [`init`] once at start-up with the EAL arguments F-Stack should use:
//!
//! ```no_run
//! # fn main() -> std::io::Result<()> {
//! let config = tokio_fstack::Config::default()
//!     .with_eal_args(["--conf", "/etc/f-stack.conf", "--proc-type=primary"]);
//! tokio_fstack::init(config)?;
//! # Ok(())
//! # }
//! ```
//!
//! Without an explicit call, the first socket operation initialises the
//! reactor with a default configuration (no EAL arguments, which makes
//! F-Stack read its default `/etc/f-stack.conf`).
//!
//! ## Limitations
//!
//! * Name resolution goes through `tokio::net::lookup_host`, i.e. the host
//!   kernel resolver. On a DPDK-only host use literal IP addresses.
//! * `send`/`write` operations that were polled to `Pending` complete with the
//!   payload they were started with; do not cancel in-flight writes from a
//!   `select!` branch expecting the newer buffer to be used.
//! * There is no `TcpSocket` builder, `UnixStream` or `TcpStream::split` by
//!   reference; `into_split` is provided.

#![warn(missing_docs)]
#![warn(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

use std::io;

mod listener;
mod ops;
mod poll_io;
mod reactor;
mod stream;
mod sys;
mod udp;
mod worker;

pub use listener::{Incoming, TcpListener};
pub use stream::{OwnedReadHalf, OwnedWriteHalf, TcpStream};
pub use udp::UdpSocket;
pub use worker::{
    multi_worker_supported, spawn_worker, WorkerBus, WorkerBusReceiver, WorkerBusSender,
    WorkerConfig, WorkerHandle,
};

/// Runtime configuration for the socket backend.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// Arguments passed to `ff_init` (F-Stack backend only).
    ///
    /// Example: `["--conf", "/etc/f-stack.conf", "--proc-type=primary"]`.
    /// The program name is prepended automatically.
    pub eal_args: Vec<String>,
}

impl Config {
    /// Create a configuration with no extra EAL arguments.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the arguments passed to `ff_init`.
    pub fn with_eal_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.eal_args = args.into_iter().map(Into::into).collect();
        self
    }
}

/// Initialise F-Stack and create the socket reactor on the **calling
/// thread**.
///
/// F-Stack's `ff_init` must run on the process main thread. Call
/// [`run_fstack_loop`] afterwards on the same thread; application/proxy code
/// runs on another thread and submits socket operations through the reactor
/// queue. [`run_with_stack`] does this for you.
#[cfg(feature = "fstack")]
pub fn init_fstack(config: Config) -> io::Result<()> {
    reactor::init_fstack(config)
}

/// Run the F-Stack event loop on the calling thread until it is stopped.
///
/// Must be called on the same thread as [`init_fstack`].
#[cfg(feature = "fstack")]
pub fn run_fstack_loop() -> io::Result<()> {
    reactor::run_fstack_loop()
}

/// Initialise the socket backend and run `app`.
///
/// * Standard backend: starts the reactor and calls `app` on the current
///   thread.
/// * F-Stack backend: calls `ff_init` on the current (main) thread, spawns
///   `app` on a dedicated thread, and then runs `ff_run` on the current
///   thread. When `app` returns, the event loop is stopped and the result is
///   propagated.
///
/// This is the recommended entry point for F-Stack applications, because
/// `ff_init`/`ff_run` must not be called from a worker thread.
pub fn run_with_stack<F>(config: Config, app: F) -> io::Result<()>
where
    F: FnOnce() -> io::Result<()> + Send + 'static,
{
    #[cfg(feature = "fstack")]
    {
        init_fstack(config)?;
        let worker = std::thread::Builder::new()
            .name("mc-proxy-app".to_owned())
            .spawn(move || {
                let result = app();
                reactor::stop_fstack_loop();
                result
            })?;
        run_fstack_loop()?;
        match worker.join() {
            Ok(result) => result,
            Err(_) => Err(io::Error::other("application thread panicked")),
        }
    }
    #[cfg(not(feature = "fstack"))]
    {
        init(config)?;
        app()
    }
}

/// Initialise the socket backend.
///
/// For the F-Stack backend this is only valid after [`init_fstack`], because
/// `ff_init`/`ff_run` must run on the process main thread; prefer
/// [`run_with_stack`], which sets this up correctly. It is idempotent; only the
/// first call has an effect.
///
/// On the standard backend this starts the readiness driver thread and ignores
/// [`Config::eal_args`].
pub fn init(config: Config) -> io::Result<()> {
    reactor::start(config)
}

/// Returns `true` when this build uses the F-Stack backend.
pub const fn is_fstack() -> bool {
    cfg!(feature = "fstack")
}

/// Returns the name of the active backend.
pub const fn backend() -> &'static str {
    if cfg!(feature = "fstack") {
        "f-stack"
    } else {
        "std"
    }
}
