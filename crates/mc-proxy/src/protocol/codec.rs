//! Primitive codecs: VarInt, String, UUID and friends.

use bytes::Bytes;
use uuid::Uuid;

use super::{ProtocolError, Result};

/// Maximum length of a VarInt in bytes.
pub const MAX_VARINT_BYTES: usize = 5;

/// Append `value` to `out` as a VarInt.
pub fn write_varint(out: &mut Vec<u8>, value: i32) {
    let mut value = value as u32;
    loop {
        if value & !0x7F == 0 {
            out.push(value as u8);
            return;
        }
        out.push(((value & 0x7F) | 0x80) as u8);
        value >>= 7;
    }
}

/// Number of bytes `value` occupies when encoded as a VarInt.
pub fn varint_len(value: i32) -> usize {
    let mut value = value as u32;
    let mut len = 1;
    while value & !0x7F != 0 {
        value >>= 7;
        len += 1;
    }
    len
}

/// Decode a VarInt from the start of `data`.
///
/// Returns the value and the number of bytes consumed, or `None` if `data`
/// ends in the middle of the VarInt.
pub fn try_varint(data: &[u8]) -> Result<Option<(i32, usize)>> {
    let mut value = 0u32;
    for (index, byte) in data.iter().take(MAX_VARINT_BYTES).enumerate() {
        value |= u32::from(byte & 0x7F) << (7 * index);
        if byte & 0x80 == 0 {
            return Ok(Some((value as i32, index + 1)));
        }
    }
    if data.len() >= MAX_VARINT_BYTES {
        return Err(ProtocolError::VarInt);
    }
    Ok(None)
}

/// Decode a VarInt that must be fully contained in `data`.
pub fn read_varint(data: &[u8]) -> Result<(i32, usize)> {
    try_varint(data)?.ok_or(ProtocolError::Eof)
}

/// A cursor over packet payload bytes.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Wrap a slice.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Number of unread bytes.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// The unread bytes.
    pub fn remaining_bytes(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    /// Read a single byte.
    pub fn read_u8(&mut self) -> Result<u8> {
        let byte = *self.data.get(self.pos).ok_or(ProtocolError::Eof)?;
        self.pos += 1;
        Ok(byte)
    }

    /// Read a boolean.
    pub fn read_bool(&mut self) -> Result<bool> {
        Ok(self.read_u8()? != 0)
    }

    /// Read a VarInt.
    pub fn read_varint(&mut self) -> Result<i32> {
        let (value, len) = read_varint(&self.data[self.pos..])?;
        self.pos += len;
        Ok(value)
    }

    /// Read a `u16` (big endian).
    pub fn read_u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// Read an `i64` (big endian).
    pub fn read_i64(&mut self) -> Result<i64> {
        let bytes = self.take(8)?;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(bytes);
        Ok(i64::from_be_bytes(buf))
    }

    /// Read exactly `len` bytes.
    pub fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        if self.remaining() < len {
            return Err(ProtocolError::Eof);
        }
        let bytes = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(bytes)
    }

    /// Read a VarInt-prefixed string.
    pub fn read_string(&mut self, max_chars: usize) -> Result<String> {
        let len = self.read_varint()?;
        if len < 0 {
            return Err(ProtocolError::Violation("negative string length".into()));
        }
        let len = len as usize;
        // UTF-8 never needs fewer bytes than characters, so an over-long
        // byte length is rejected before allocating.
        if len > max_chars * 4 {
            return Err(ProtocolError::StringTooLong {
                len,
                max: max_chars * 4,
            });
        }
        let bytes = self.take(len)?;
        let text = std::str::from_utf8(bytes)
            .map_err(|_| ProtocolError::Violation("invalid UTF-8 string".into()))?;
        if text.chars().count() > max_chars {
            return Err(ProtocolError::StringTooLong {
                len,
                max: max_chars,
            });
        }
        Ok(text.to_owned())
    }

    /// Read a UUID (16 bytes, big endian).
    pub fn read_uuid(&mut self) -> Result<Uuid> {
        let bytes = self.take(16)?;
        let mut raw = [0u8; 16];
        raw.copy_from_slice(bytes);
        Ok(Uuid::from_bytes(raw))
    }

    /// Read a VarInt-prefixed byte array.
    pub fn read_byte_array(&mut self, max: usize) -> Result<&'a [u8]> {
        let len = self.read_varint()?;
        if len < 0 {
            return Err(ProtocolError::Violation("negative byte array".into()));
        }
        let len = len as usize;
        if len > max {
            return Err(ProtocolError::StringTooLong { len, max });
        }
        self.take(len)
    }

    /// Read all remaining bytes.
    pub fn read_remaining(&mut self) -> &'a [u8] {
        let bytes = &self.data[self.pos..];
        self.pos = self.data.len();
        bytes
    }
}

/// Builder for packet payloads.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// Create an empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a writer with pre-allocated capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

    /// Append raw bytes.
    pub fn bytes(&mut self, bytes: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(bytes);
        self
    }

    /// Append a byte.
    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.buf.push(value);
        self
    }

    /// Append a boolean.
    pub fn bool(&mut self, value: bool) -> &mut Self {
        self.buf.push(u8::from(value));
        self
    }

    /// Append a VarInt.
    pub fn varint(&mut self, value: i32) -> &mut Self {
        write_varint(&mut self.buf, value);
        self
    }

    /// Append a `u16` (big endian).
    pub fn u16(&mut self, value: u16) -> &mut Self {
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Append an `i64` (big endian).
    pub fn i64(&mut self, value: i64) -> &mut Self {
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Append a VarInt-prefixed string.
    pub fn string(&mut self, value: &str) -> &mut Self {
        self.varint(value.len() as i32);
        self.buf.extend_from_slice(value.as_bytes());
        self
    }

    /// Append a UUID.
    pub fn uuid(&mut self, value: &Uuid) -> &mut Self {
        self.buf.extend_from_slice(value.as_bytes());
        self
    }

    /// Append a VarInt-prefixed byte array.
    pub fn byte_array(&mut self, value: &[u8]) -> &mut Self {
        self.varint(value.len() as i32);
        self.buf.extend_from_slice(value);
        self
    }

    /// Length of the encoded data.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing was written.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Consume the writer, returning the encoded bytes.
    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

/// A complete packet: id plus payload.
#[derive(Debug, Clone)]
pub struct Packet {
    /// Packet id.
    pub id: i32,
    /// Payload bytes (without the id).
    pub data: Bytes,
}

impl Packet {
    /// Create a packet.
    pub fn new(id: i32, data: impl Into<Bytes>) -> Self {
        Self {
            id,
            data: data.into(),
        }
    }

    /// Create a packet from an encoded writer.
    pub fn from_writer(id: i32, writer: Writer) -> Self {
        Self {
            id,
            data: Bytes::from(writer.into_inner()),
        }
    }

    /// A reader over the payload.
    pub fn reader(&self) -> Reader<'_> {
        Reader::new(&self.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        let values = [
            0,
            1,
            2,
            127,
            128,
            255,
            300,
            25565,
            2097151,
            2147483647,
            -1,
            -2147483648,
        ];
        for value in values {
            let mut buf = Vec::new();
            write_varint(&mut buf, value);
            assert_eq!(buf.len(), varint_len(value), "length mismatch for {value}");
            let (decoded, len) = read_varint(&buf).unwrap();
            assert_eq!(decoded, value);
            assert_eq!(len, buf.len());
        }
    }

    #[test]
    fn varint_rejects_garbage() {
        assert!(matches!(
            try_varint(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01]),
            Err(ProtocolError::VarInt)
        ));
        assert!(try_varint(&[0x80, 0x80]).unwrap().is_none());
    }

    #[test]
    fn strings_and_arrays() {
        let mut writer = Writer::new();
        writer.string("hello").u16(25565).bool(true).i64(-42);
        let data = writer.into_inner();
        let mut reader = Reader::new(&data);
        assert_eq!(reader.read_string(16).unwrap(), "hello");
        assert_eq!(reader.read_u16().unwrap(), 25565);
        assert!(reader.read_bool().unwrap());
        assert_eq!(reader.read_i64().unwrap(), -42);
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn rejects_oversized_strings() {
        let mut writer = Writer::new();
        writer.varint(64).bytes(&[b'a'; 64]);
        let data = writer.into_inner();
        let mut reader = Reader::new(&data);
        assert!(matches!(
            reader.read_string(10),
            Err(ProtocolError::StringTooLong { .. })
        ));
    }
}
