//! `mc-proxy`: a Velocity-compatible Minecraft reverse proxy built on
//! `tokio-fstack`.
//!
//! The crate is split into reusable pieces:
//!
//! * [`protocol`] — VarInts, framing, compression, handshake/status/login
//!   packets.
//! * [`forwarding`] — Velocity "modern" player information forwarding.
//! * [`crypto`] — RSA/AES for online mode, offline UUIDs and Minecraft's
//!   signed SHA-1 server hash.
//! * [`auth`] — `hasJoined` session server client (HTTP/HTTPS over
//!   `tokio-fstack` sockets).
//! * [`proxy`] — listener, routing, session state machine and relay.

#![warn(missing_docs)]

pub mod auth;
pub mod config;
pub mod crypto;
pub mod forwarding;
pub mod protocol;
pub mod proxy;
pub mod servers;
pub mod sockets;

pub use config::Config;
pub use proxy::{Proxy, ProxyError, Shared};
