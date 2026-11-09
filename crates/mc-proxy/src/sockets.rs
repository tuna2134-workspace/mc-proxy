//! Socket layer used by the proxy.
//!
//! This is a thin alias over [`tokio_fstack`] so that switching backends is a
//! one-line change and so that the rest of the crate does not depend on the
//! concrete type names.

pub use tokio_fstack::{
    is_fstack, OwnedReadHalf, OwnedWriteHalf, TcpListener, TcpStream, UdpSocket,
};

/// Initialise the socket backend.
///
/// `eal_args` is only used by the F-Stack backend; the standard backend
/// ignores it.
pub fn init(eal_args: &[String]) -> std::io::Result<()> {
    let mut config = tokio_fstack::Config::default();
    if !eal_args.is_empty() {
        config = config.with_eal_args(eal_args.iter().cloned());
    }
    tokio_fstack::init(config)
}
