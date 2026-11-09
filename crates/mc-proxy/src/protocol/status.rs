//! Server-list ping (status) packets and response construction.

use serde_json::json;
use uuid::Uuid;

use super::codec::{Packet, Reader, Writer};
use super::Result;

/// Serverbound Status Request id.
pub const STATUS_REQUEST_ID: i32 = 0x00;
/// Serverbound Ping Request id.
pub const PING_REQUEST_ID: i32 = 0x01;
/// Clientbound Status Response id.
pub const STATUS_RESPONSE_ID: i32 = 0x00;
/// Clientbound Pong Response id.
pub const PONG_RESPONSE_ID: i32 = 0x01;

/// Ping payload (an opaque `i64` echoed back by the proxy).
pub fn decode_ping(packet: &Packet) -> Result<i64> {
    Reader::new(&packet.data).read_i64()
}

/// Encode a Pong Response payload.
pub fn encode_pong(payload: i64) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.i64(payload);
    writer.into_inner()
}

/// Encode a Status Response payload.
pub fn encode_status_response(json: &str) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.string(json);
    writer.into_inner()
}

/// Convert `&a`-style colour codes to the section-sign form clients expect.
pub fn convert_color_codes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '&' {
            if let Some(next) = chars.peek().copied() {
                if "0123456789abcdefklmnorABCDEFKLMNOR".contains(next) {
                    out.push('§');
                    out.push(next);
                    chars.next();
                    continue;
                }
            }
        }
        out.push(ch);
    }
    out
}

/// Everything needed to build a status response.
pub struct StatusResponse {
    /// Version name shown to the client.
    pub version_name: String,
    /// Protocol version the proxy reports as compatible.
    pub protocol: i32,
    /// Maximum player count.
    pub max_players: usize,
    /// Current player count.
    pub online_players: usize,
    /// Player sample entries.
    pub sample: Vec<(String, Uuid)>,
    /// MOTD, may contain newlines and colour codes.
    pub motd: String,
    /// Optional favicon as a `data:image/png;base64,...` URI.
    pub favicon: Option<String>,
    /// Whether secure chat is enforced.
    pub enforces_secure_chat: bool,
}

impl StatusResponse {
    /// Serialise to the JSON document sent in a Status Response.
    pub fn to_json(&self) -> String {
        let description = json!({
            "text": convert_color_codes(&self.motd),
        });
        let sample: Vec<serde_json::Value> = self
            .sample
            .iter()
            .take(12)
            .map(|(name, id)| json!({ "name": name, "id": id.to_string() }))
            .collect();
        let mut value = json!({
            "version": {
                "name": self.version_name,
                "protocol": self.protocol,
            },
            "players": {
                "max": self.max_players,
                "online": self.online_players,
                "sample": sample,
            },
            "description": description,
            "enforcesSecureChat": self.enforces_secure_chat,
            "previewsChat": false,
        });
        if let Some(favicon) = &self.favicon {
            value["favicon"] = json!(favicon);
        }
        value.to_string()
    }
}

/// Build a legacy (pre-1.7) ping response.
///
/// `modern` selects the 1.6 format (`§1\0proto\0version\0motd\0online\0max`);
/// otherwise the 1.4/1.5 format (`motd§online§max`) is used. Both are encoded
/// as UTF-16BE and prefixed with `0xFF` and the length in UTF-16 code units.
pub fn legacy_ping_response(
    protocol: i32,
    version: &str,
    motd: &str,
    online: usize,
    max: usize,
    modern: bool,
) -> Vec<u8> {
    let text = if modern {
        format!(
            "§1\0{}\0{}\0{}\0{}\0{}",
            protocol,
            version,
            convert_color_codes(motd),
            online,
            max
        )
    } else {
        format!("{}§{}§{}", convert_color_codes(motd), online, max)
    };
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut out = Vec::with_capacity(3 + units.len() * 2);
    out.push(0xFF);
    out.extend_from_slice(&(units.len() as u16).to_be_bytes());
    for unit in units {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    out
}

/// Whether a status request is the legacy `0xFE` ping.
pub fn is_legacy_ping(first_byte: u8) -> bool {
    first_byte == 0xFE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_codes() {
        assert_eq!(convert_color_codes("&aHello &lWorld"), "§aHello §lWorld");
        assert_eq!(convert_color_codes("100% & more"), "100% & more");
    }

    #[test]
    fn status_json_contains_expected_fields() {
        let response = StatusResponse {
            version_name: "mc-proxy".into(),
            protocol: 770,
            max_players: 100,
            online_players: 3,
            sample: vec![("Notch".into(), Uuid::nil())],
            motd: "&aHello\n&bWorld".into(),
            favicon: Some("data:image/png;base64,AAAA".into()),
            enforces_secure_chat: false,
        };
        let json = response.to_json();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["version"]["protocol"], 770);
        assert_eq!(value["players"]["online"], 3);
        assert_eq!(value["description"]["text"], "§aHello\n§bWorld");
        assert_eq!(value["favicon"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn legacy_ping_is_utf16() {
        let response = legacy_ping_response(127, "1.6.4", "Hi", 1, 20, true);
        assert_eq!(response[0], 0xFF);
        let units = u16::from_be_bytes([response[1], response[2]]) as usize;
        assert_eq!(response.len(), 3 + units * 2);
        let text: Vec<u16> = response[3..]
            .chunks_exact(2)
            .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
            .collect();
        let text = String::from_utf16(&text).unwrap();
        assert!(text.starts_with("§1\u{0}127\u{0}1.6.4\u{0}Hi\u{0}1\u{0}20"));
    }
}
