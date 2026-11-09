//! Velocity "modern" player information forwarding.
//!
//! The proxy answers a `velocity:player_info` login plugin request from the
//! backend with:
//!
//! ```text
//! [32-byte HMAC-SHA256] [payload]
//! payload = VarInt version || String remote address || UUID
//!           || String username || VarInt property count
//!           || { String name || String value || bool || String signature }*
//! ```
//!
//! The backend verifies the HMAC with the shared secret, then uses the address,
//! UUID and properties instead of trusting the connection.
//!
//! Reference: `PlayerDataForwarding.createForwardingData` in Velocity and
//! `VelocityProxy.checkIntegrity` in Paper.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

use crate::protocol::codec::{Reader, Writer};
use crate::protocol::login::{decode_properties, encode_properties, Property};
use crate::protocol::{ProtocolError, Result};

/// Forwarding version 1: address, UUID, name and properties.
pub const MODERN_FORWARDING_VERSION: u8 = 1;

/// Data forwarded to the backend for a logged-in player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardingData {
    /// Remote address as string (IPv4 or IPv6 literal).
    pub address: String,
    /// Authenticated (or offline) player UUID.
    pub uuid: Uuid,
    /// Player name.
    pub name: String,
    /// Skin/profile properties.
    pub properties: Vec<Property>,
}

/// Build the signed forwarding payload.
pub fn create_forwarding_data(
    secret: &[u8],
    data: &ForwardingData,
    version: u8,
) -> Result<Vec<u8>> {
    let payload = encode_payload(data, version);
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret)
        .map_err(|_| ProtocolError::Violation("invalid forwarding secret".into()))?;
    mac.update(&payload);
    let signature = mac.finalize().into_bytes();
    let mut out = Vec::with_capacity(32 + payload.len());
    out.extend_from_slice(&signature);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Encode the unsigned forwarding payload.
pub fn encode_payload(data: &ForwardingData, version: u8) -> Vec<u8> {
    let mut writer = Writer::new();
    writer
        .varint(i32::from(version))
        .string(&data.address)
        .uuid(&data.uuid)
        .string(&data.name);
    encode_properties(&mut writer, &data.properties);
    writer.into_inner()
}

/// Verify and decode a forwarding response payload, as a backend would.
///
/// Returns `Ok(None)` when the signature does not match.
pub fn verify_forwarding_data(
    secret: &[u8],
    signed: &[u8],
) -> Result<Option<(u8, ForwardingData)>> {
    if signed.len() < 32 {
        return Err(ProtocolError::Violation(
            "forwarding data shorter than its signature".into(),
        ));
    }
    let (signature, payload) = signed.split_at(32);
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret)
        .map_err(|_| ProtocolError::Violation("invalid forwarding secret".into()))?;
    mac.update(payload);
    if mac.verify_slice(signature).is_err() {
        return Ok(None);
    }
    let mut reader = Reader::new(payload);
    let version = reader.read_varint()?;
    if version < 1 {
        return Err(ProtocolError::Violation(
            "invalid forwarding version".into(),
        ));
    }
    let address = reader.read_string(255)?;
    let uuid = reader.read_uuid()?;
    let name = reader.read_string(16)?;
    let properties = decode_properties(&mut reader)?;
    Ok(Some((
        version as u8,
        ForwardingData {
            address,
            uuid,
            name,
            properties,
        },
    )))
}

/// Decode just the requested forwarding version from a login plugin request.
///
/// Backends send a single byte with the highest version they support; an empty
/// payload means version 1.
pub fn requested_version(data: &[u8]) -> u8 {
    match data {
        [version, ..] if *version >= 1 => *version,
        _ => MODERN_FORWARDING_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ForwardingData {
        ForwardingData {
            address: "203.0.113.7".into(),
            uuid: Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
            name: "Notch".into(),
            properties: vec![Property {
                name: "textures".into(),
                value: "dGV4dHVyZXM=".into(),
                signature: Some("c2lnbmF0dXJl".into()),
            }],
        }
    }

    #[test]
    fn payload_layout_matches_velocity() {
        let payload = encode_payload(&sample(), 1);
        let mut reader = Reader::new(&payload);
        assert_eq!(reader.read_varint().unwrap(), 1);
        assert_eq!(reader.read_string(255).unwrap(), "203.0.113.7");
        assert_eq!(
            reader.read_uuid().unwrap(),
            Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef)
        );
        assert_eq!(reader.read_string(16).unwrap(), "Notch");
        let properties = decode_properties(&mut reader).unwrap();
        assert_eq!(properties.len(), 1);
        assert_eq!(properties[0].name, "textures");
    }

    #[test]
    fn signature_is_verified_and_data_roundtrips() {
        let data = sample();
        let signed = create_forwarding_data(b"super-secret", &data, 1).unwrap();
        let (version, parsed) = verify_forwarding_data(b"super-secret", &signed)
            .unwrap()
            .expect("signature should verify");
        assert_eq!(version, 1);
        assert_eq!(parsed, data);

        assert!(verify_forwarding_data(b"wrong-secret", &signed)
            .unwrap()
            .is_none());
    }

    #[test]
    fn hmac_matches_independent_implementation() {
        // HMAC-SHA256("secret", payload) computed with Python's hmac module
        // for the payload of `sample()` with version 1.
        let signed = create_forwarding_data(b"secret", &sample(), 1).unwrap();
        let signature = hex(&signed[..32]);
        assert_eq!(
            signature,
            "84f2b54be6304b2727bddf77a4988c31925cd7aa0fc98c352e15867f3bafb48a"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
