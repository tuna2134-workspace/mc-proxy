//! Login-state packets.
//!
//! Packet ids in the login state have been stable since 1.13 (protocol 393),
//! which is the minimum version this proxy accepts.

use uuid::Uuid;

use super::codec::{Packet, Reader, Writer};
use super::Result;

/// Serverbound packet ids.
pub mod serverbound {
    /// Login Start.
    pub const LOGIN_START: i32 = 0x00;
    /// Encryption Response.
    pub const ENCRYPTION_RESPONSE: i32 = 0x01;
    /// Login Plugin Response.
    pub const PLUGIN_RESPONSE: i32 = 0x02;
    /// Login Acknowledged (1.20.2+).
    pub const LOGIN_ACKNOWLEDGED: i32 = 0x03;
    /// Cookie Response (1.20.5+).
    pub const COOKIE_RESPONSE: i32 = 0x04;
}

/// Clientbound packet ids.
pub mod clientbound {
    /// Login Disconnect.
    pub const DISCONNECT: i32 = 0x00;
    /// Encryption Request.
    pub const ENCRYPTION_REQUEST: i32 = 0x01;
    /// Login Success.
    pub const LOGIN_SUCCESS: i32 = 0x02;
    /// Set Compression.
    pub const SET_COMPRESSION: i32 = 0x03;
    /// Login Plugin Request.
    pub const PLUGIN_REQUEST: i32 = 0x04;
    /// Cookie Request (1.20.5+).
    pub const COOKIE_REQUEST: i32 = 0x05;
}

/// Channel name used by Velocity modern forwarding.
pub const VELOCITY_PLAYER_INFO_CHANNEL: &str = "velocity:player_info";

/// Decoded Login Start.
#[derive(Debug, Clone)]
pub struct LoginStart {
    /// Player name.
    pub name: String,
    /// UUID included by 1.20.2+ clients.
    pub uuid: Option<Uuid>,
}

impl LoginStart {
    /// Decode a Login Start packet.
    pub fn decode(packet: &Packet, protocol: i32) -> Result<Self> {
        if packet.id != serverbound::LOGIN_START {
            return Err(super::ProtocolError::UnexpectedPacket {
                id: packet.id,
                state: "login",
            });
        }
        let mut reader = packet.reader();
        let name = reader.read_string(16)?;
        // 1.20.2 (764) added the player UUID to Login Start; older versions
        // either end here or carry 1.19 profile-key data which the proxy does
        // not need.
        let uuid = if protocol >= 764 && reader.remaining() >= 16 {
            Some(reader.read_uuid()?)
        } else {
            None
        };
        Ok(Self { name, uuid })
    }
}

/// Decoded Encryption Response.
#[derive(Debug, Clone)]
pub struct EncryptionResponse {
    /// RSA-encrypted shared secret.
    pub shared_secret: Vec<u8>,
    /// RSA-encrypted verify token.
    pub verify_token: Vec<u8>,
}

impl EncryptionResponse {
    /// Decode an Encryption Response packet.
    pub fn decode(packet: &Packet) -> Result<Self> {
        if packet.id != serverbound::ENCRYPTION_RESPONSE {
            return Err(super::ProtocolError::UnexpectedPacket {
                id: packet.id,
                state: "login/encryption",
            });
        }
        let mut reader = packet.reader();
        let shared_secret = reader.read_byte_array(512)?.to_vec();
        let verify_token = reader.read_byte_array(512)?.to_vec();
        Ok(Self {
            shared_secret,
            verify_token,
        })
    }
}

/// Decoded Login Plugin Response.
#[derive(Debug, Clone)]
pub struct PluginResponse {
    /// Message id echoed from the request.
    pub message_id: i32,
    /// Whether the client understood the request.
    pub successful: bool,
    /// Response payload.
    pub data: Vec<u8>,
}

impl PluginResponse {
    /// Decode a Login Plugin Response packet.
    pub fn decode(packet: &Packet) -> Result<Self> {
        let mut reader = packet.reader();
        let message_id = reader.read_varint()?;
        let successful = reader.read_bool()?;
        let data = reader.read_remaining().to_vec();
        Ok(Self {
            message_id,
            successful,
            data,
        })
    }
}

/// Decoded Login Plugin Request.
#[derive(Debug, Clone)]
pub struct PluginRequest {
    /// Message id to echo back.
    pub message_id: i32,
    /// Plugin channel.
    pub channel: String,
    /// Request payload.
    pub data: Vec<u8>,
}

impl PluginRequest {
    /// Decode a Login Plugin Request packet.
    pub fn decode(packet: &Packet) -> Result<Self> {
        let mut reader = packet.reader();
        let message_id = reader.read_varint()?;
        let channel = reader.read_string(256)?;
        let data = reader.read_remaining().to_vec();
        Ok(Self {
            message_id,
            channel,
            data,
        })
    }
}

/// Decode the chat JSON of a Disconnect packet into plain text.
pub fn decode_disconnect_reason(packet: &Packet) -> String {
    let mut reader = Reader::new(&packet.data);
    let Ok(json) = reader.read_string(32_767) else {
        return "Disconnected".to_owned();
    };
    match serde_json::from_str::<serde_json::Value>(&json) {
        Ok(value) => {
            let text = chat_plain_text(&value);
            if text.is_empty() {
                json
            } else {
                text
            }
        }
        Err(_) => json,
    }
}

fn chat_plain_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => items.iter().map(chat_plain_text).collect(),
        serde_json::Value::Object(map) => {
            let mut text = map
                .get("text")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_owned();
            if let Some(extra) = map.get("extra").and_then(|value| value.as_array()) {
                for item in extra {
                    text.push_str(&chat_plain_text(item));
                }
            }
            text
        }
        _ => String::new(),
    }
}

/// Decode the threshold of a Set Compression packet.
pub fn decode_set_compression(packet: &Packet) -> Result<u32> {
    let threshold = Reader::new(&packet.data).read_varint()?;
    if threshold < 0 {
        return Ok(0);
    }
    Ok(threshold as u32)
}

/// Encode a clientbound Login Disconnect carrying `reason` as chat JSON.
pub fn encode_disconnect(reason_json: &str) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.string(reason_json);
    writer.into_inner()
}

/// Encode a serverbound Login Start.
pub fn encode_login_start(name: &str, uuid: Option<&Uuid>, protocol: i32) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.string(name);
    if protocol >= 764 {
        writer.uuid(uuid.unwrap_or(&Uuid::nil()));
    }
    writer.into_inner()
}

/// Protocol version that added the `Should Authenticate` boolean to the
/// Encryption Request (Minecraft 1.20.5, protocol 766).
///
/// Note: 1.19.3 (761) did **not** add this field; it only changed Login Start
/// profile-key handling. Sending the boolean to 1.19.3–1.20.4 clients
/// corrupts their decode, and omitting it for 1.20.5+ clients corrupts theirs.
pub const SHOULD_AUTHENTICATE_PROTOCOL: i32 = 766;

/// Encode an Encryption Request.
///
/// `Should Authenticate` is only present for protocol >= 1.20.5 (766). The
/// proxy always sends `true` because it verifies the player with the Mojang
/// session server.
pub fn encode_encryption_request(
    server_id: &str,
    public_key: &[u8],
    verify_token: &[u8],
    protocol_version: i32,
    should_authenticate: bool,
) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.string(server_id);
    writer.byte_array(public_key);
    writer.byte_array(verify_token);
    if protocol_version >= SHOULD_AUTHENTICATE_PROTOCOL {
        writer.bool(should_authenticate);
    }
    writer.into_inner()
}

/// Encode a Login Success for protocols >= 1.16 (735).
pub fn encode_login_success(uuid: &Uuid, name: &str, properties: &[Property]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.uuid(uuid);
    writer.string(name);
    encode_properties(&mut writer, properties);
    writer.into_inner()
}

/// Encode a Login Plugin Response.
pub fn encode_plugin_response(message_id: i32, successful: bool, data: &[u8]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.varint(message_id).bool(successful).bytes(data);
    writer.into_inner()
}

/// Encode a Login Plugin Request.
pub fn encode_plugin_request(message_id: i32, channel: &str, data: &[u8]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.varint(message_id).string(channel).bytes(data);
    writer.into_inner()
}

/// Encode a Set Compression packet.
pub fn encode_set_compression(threshold: i32) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.varint(threshold);
    writer.into_inner()
}

/// Decode a VarInt-prefixed list of profile properties.
pub fn decode_properties(reader: &mut Reader<'_>) -> Result<Vec<Property>> {
    let count = reader.read_varint()?;
    if count < 0 {
        return Err(super::ProtocolError::Violation(
            "negative property count".into(),
        ));
    }
    if count > 1024 {
        return Err(super::ProtocolError::Violation(
            "too many profile properties".into(),
        ));
    }
    let mut properties = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let name = reader.read_string(256)?;
        let value = reader.read_string(32_767)?;
        let signature = if reader.read_bool()? {
            Some(reader.read_string(32_767)?)
        } else {
            None
        };
        properties.push(Property {
            name,
            value,
            signature,
        });
    }
    Ok(properties)
}

/// Encode a list of profile properties with its VarInt count.
pub fn encode_properties(writer: &mut Writer, properties: &[Property]) {
    writer.varint(properties.len() as i32);
    for property in properties {
        writer.string(&property.name);
        writer.string(&property.value);
        match &property.signature {
            Some(signature) => {
                writer.bool(true);
                writer.string(signature);
            }
            None => {
                writer.bool(false);
            }
        }
    }
}

/// A game profile property (skin textures and friends).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    /// Property name, e.g. `textures`.
    pub name: String,
    /// Property value (base64 for textures).
    pub value: String,
    /// Optional Mojang signature.
    pub signature: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_start_roundtrip() {
        let uuid = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
        let payload = encode_login_start("Notch", Some(&uuid), 770);
        let packet = Packet::new(serverbound::LOGIN_START, payload);
        let start = LoginStart::decode(&packet, 770).unwrap();
        assert_eq!(start.name, "Notch");
        assert_eq!(start.uuid, Some(uuid));
    }

    #[test]
    fn login_start_without_uuid() {
        let mut writer = Writer::new();
        writer.string("Alex");
        let packet = Packet::from_writer(serverbound::LOGIN_START, writer);
        let start = LoginStart::decode(&packet, 763).unwrap();
        assert_eq!(start.name, "Alex");
        assert_eq!(start.uuid, None);
    }

    #[test]
    fn encryption_response_decodes() {
        let mut writer = Writer::new();
        writer.byte_array(&[1u8; 128]).byte_array(&[2u8; 128]);
        let packet = Packet::from_writer(serverbound::ENCRYPTION_RESPONSE, writer);
        let response = EncryptionResponse::decode(&packet).unwrap();
        assert_eq!(response.shared_secret.len(), 128);
        assert_eq!(response.verify_token.len(), 128);
    }

    #[test]
    fn login_success_has_properties() {
        let uuid = Uuid::nil();
        let properties = vec![Property {
            name: "textures".into(),
            value: "abc".into(),
            signature: Some("sig".into()),
        }];
        let payload = encode_login_success(&uuid, "Notch", &properties);
        let mut reader = Reader::new(&payload);
        assert_eq!(reader.read_uuid().unwrap(), uuid);
        assert_eq!(reader.read_string(16).unwrap(), "Notch");
        assert_eq!(reader.read_varint().unwrap(), 1);
        assert_eq!(reader.read_string(64).unwrap(), "textures");
        assert_eq!(reader.read_string(1024).unwrap(), "abc");
        assert!(reader.read_bool().unwrap());
        assert_eq!(reader.read_string(1024).unwrap(), "sig");
    }

    #[test]
    fn encryption_request_gates_should_authenticate_on_1_20_5() {
        // 1.19.3 (761), 1.20.2 (764) and 1.20.4 (765) must NOT carry the
        // boolean; only 1.20.5 (766)+ does.
        for protocol in [393, 761, 764, 765] {
            let payload = encode_encryption_request("", &[1, 2, 3], &[4, 5, 6, 7], protocol, true);
            let mut reader = Reader::new(&payload);
            assert_eq!(reader.read_string(20).unwrap(), "");
            assert_eq!(reader.read_byte_array(512).unwrap(), &[1, 2, 3]);
            assert_eq!(reader.read_byte_array(512).unwrap(), &[4, 5, 6, 7]);
            assert_eq!(
                reader.remaining(),
                0,
                "Should Authenticate must be absent before 1.20.5 (protocol {protocol})"
            );
        }
        for protocol in [766, 767, 770, 771] {
            let payload = encode_encryption_request("", &[1, 2, 3], &[4, 5, 6, 7], protocol, true);
            let mut reader = Reader::new(&payload);
            assert_eq!(reader.read_string(20).unwrap(), "");
            assert_eq!(reader.read_byte_array(512).unwrap(), &[1, 2, 3]);
            assert_eq!(reader.read_byte_array(512).unwrap(), &[4, 5, 6, 7]);
            assert!(
                reader.read_bool().unwrap(),
                "Should Authenticate must be true for protocol {protocol}"
            );
            assert_eq!(reader.remaining(), 0);
        }
    }

    #[test]
    fn encryption_request_can_disable_authentication() {
        let payload = encode_encryption_request("", &[9], &[8], 770, false);
        let mut reader = Reader::new(&payload);
        assert_eq!(reader.read_string(20).unwrap(), "");
        assert_eq!(reader.read_byte_array(512).unwrap(), &[9]);
        assert_eq!(reader.read_byte_array(512).unwrap(), &[8]);
        assert!(!reader.read_bool().unwrap());
        assert_eq!(reader.remaining(), 0);
    }
}
