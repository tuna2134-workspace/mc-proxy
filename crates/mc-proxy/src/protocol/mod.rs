//! Minecraft Java Edition protocol primitives.
//!
//! Only the parts needed by a proxy are implemented: VarInts, strings, UUIDs,
//! packet framing with zlib compression, and the state machine packets for
//! handshake, status and login. Everything in the play/configuration states is
//! relayed opaquely, which is what makes the proxy version-agnostic between
//! the supported protocol versions.

pub mod cipher;
pub mod codec;
pub mod frame;
pub mod handshake;
pub mod login;
pub mod status;

use std::io;

use thiserror::Error;

/// Errors produced while decoding or encoding protocol data.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// The peer closed the connection or an I/O error occurred.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    /// A packet ended before all fields were read.
    #[error("unexpected end of packet")]
    Eof,
    /// A VarInt was malformed or too long.
    #[error("malformed VarInt")]
    VarInt,
    /// A string was longer than the protocol allows.
    #[error("string too long: {len} > {max}")]
    StringTooLong {
        /// Actual length in bytes.
        len: usize,
        /// Maximum allowed length.
        max: usize,
    },
    /// A frame exceeded the configured maximum size.
    #[error("frame too large: {len} > {max}")]
    FrameTooLarge {
        /// Advertised frame length.
        len: usize,
        /// Configured maximum.
        max: usize,
    },
    /// The compressed payload did not match its advertised length.
    #[error("decompression failed: {0}")]
    Compression(String),
    /// The peer sent a packet the state machine did not expect.
    #[error("unexpected packet 0x{id:02x} in {state}")]
    UnexpectedPacket {
        /// Packet id.
        id: i32,
        /// Protocol state.
        state: &'static str,
    },
    /// The connection is in an invalid state for the requested operation.
    #[error("protocol violation: {0}")]
    Violation(String),
}

/// Result alias for protocol operations.
pub type Result<T> = std::result::Result<T, ProtocolError>;

impl ProtocolError {
    /// Whether this error means the peer went away.
    pub fn is_disconnect(&self) -> bool {
        match self {
            ProtocolError::Io(err) => matches!(
                err.kind(),
                io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
            ),
            ProtocolError::Eof => true,
            _ => false,
        }
    }
}
