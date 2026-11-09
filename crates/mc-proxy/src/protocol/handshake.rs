//! Handshake and version handling.

use super::codec::{Packet, Writer};
use super::Result;

/// Packet id of the initial handshake.
pub const HANDSHAKE_ID: i32 = 0x00;

/// Oldest protocol version accepted by the proxy (Minecraft 1.13).
///
/// 1.13 introduced the login plugin message that Velocity's modern forwarding
/// is built on. Newer versions (including future ones) are relayed opaquely.
pub const MIN_PROTOCOL: i32 = 393;

/// Newest protocol version the proxy explicitly knows about. Versions above
/// this are still accepted because the proxy relays play packets verbatim.
pub const MAX_PROTOCOL: i32 = 1000;

/// The state the client wants to enter after the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextState {
    /// Server list ping.
    Status,
    /// Login.
    Login,
    /// Transfer between servers (1.20.5+); treated like a login.
    Transfer,
}

/// Decoded handshake packet.
#[derive(Debug, Clone)]
pub struct Handshake {
    /// Protocol version chosen by the client.
    pub protocol_version: i32,
    /// Virtual host the client connected to (Forge markers removed).
    pub server_address: String,
    /// Virtual port.
    pub server_port: u16,
    /// Requested next state.
    pub next_state: NextState,
}

impl Handshake {
    /// Decode the handshake packet payload.
    pub fn decode(packet: &Packet) -> Result<Self> {
        if packet.id != HANDSHAKE_ID {
            return Err(super::ProtocolError::UnexpectedPacket {
                id: packet.id,
                state: "handshake",
            });
        }
        let mut reader = packet.reader();
        let protocol_version = reader.read_varint()?;
        let raw_address = reader.read_string(255)?;
        let server_port = reader.read_u16()?;
        let next_state = match reader.read_varint()? {
            1 => NextState::Status,
            2 => NextState::Login,
            3 => NextState::Transfer,
            other => {
                return Err(super::ProtocolError::Violation(format!(
                    "invalid next state {other}"
                )))
            }
        };
        Ok(Self {
            protocol_version,
            server_address: clean_address(&raw_address),
            server_port,
            next_state,
        })
    }

    /// Encode the handshake to a payload writer.
    pub fn encode(&self) -> Writer {
        let mut writer = Writer::new();
        writer
            .varint(self.protocol_version)
            .string(&self.server_address)
            .u16(self.server_port)
            .varint(match self.next_state {
                NextState::Status => 1,
                NextState::Login => 2,
                NextState::Transfer => 3,
            });
        writer
    }
}

/// Handshake used when connecting to a backend server.
pub fn backend_handshake(protocol_version: i32, address: &str, port: u16) -> Handshake {
    Handshake {
        protocol_version,
        server_address: address.to_owned(),
        server_port: port,
        next_state: NextState::Login,
    }
}

/// Remove Forge/bungee markers and normalise the virtual host.
pub fn clean_address(address: &str) -> String {
    let host = address.split('\0').next().unwrap_or(address);
    let host = host.trim_end_matches('.');
    host.to_ascii_lowercase()
}

/// Whether the protocol version is supported by the proxy.
pub fn is_supported_protocol(protocol: i32) -> bool {
    (MIN_PROTOCOL..=MAX_PROTOCOL).contains(&protocol)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_handshake() {
        let mut writer = Writer::new();
        writer
            .varint(770)
            .string("example.com")
            .u16(25565)
            .varint(2);
        let packet = Packet::from_writer(HANDSHAKE_ID, writer);
        let handshake = Handshake::decode(&packet).unwrap();
        assert_eq!(handshake.protocol_version, 770);
        assert_eq!(handshake.server_address, "example.com");
        assert_eq!(handshake.server_port, 25565);
        assert_eq!(handshake.next_state, NextState::Login);
    }

    #[test]
    fn strips_forge_markers() {
        assert_eq!(
            clean_address("Play.Example.COM.\u{0}FML\u{0}"),
            "play.example.com"
        );
        assert_eq!(clean_address("example.com"), "example.com");
    }
}
