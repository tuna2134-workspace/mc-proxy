//! Packet framing: VarInt length prefix, optional zlib compression and
//! optional AES-CFB8 encryption.
//!
//! The wire format is the standard Minecraft one:
//!
//! ```text
//! uncompressed:  [len][packet id][data]
//! compressed:    [len][data_len][zlib(packet id + data)]   (data_len != 0)
//!                [len][0][packet id + data]                (data_len == 0)
//! ```
//!
//! Encryption (when enabled during login) is applied to the whole frame,
//! including the length prefix, so it must wrap the framing layer as done
//! here.

use std::io;

use bytes::{Buf, Bytes, BytesMut};
use flate2::{
    Compress, Compression as FlateLevel, Decompress, FlushCompress, FlushDecompress, Status,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::cipher::Cfb8;
use super::codec::{read_varint, try_varint, write_varint, Reader};
use super::{ProtocolError, Result};

/// Maximum frame length accepted from a peer (8 MiB).
pub const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;
/// Maximum decompressed payload size (8 MiB).
pub const MAX_UNCOMPRESSED_LEN: usize = 8 * 1024 * 1024;
/// Size of the scratch buffer used for socket reads.
const READ_CHUNK: usize = 16 * 1024;
/// Upper bound on compression loop iterations (defensive).
const MAX_ZLIB_ITERATIONS: usize = 1024;

/// AES-128 key and IV used for the post-login stream cipher.
///
/// Minecraft uses AES/CFB8 with the shared secret as both key and IV.
#[derive(Clone)]
pub struct CipherKeys {
    /// 16-byte AES key.
    pub key: [u8; 16],
    /// 16-byte initialisation vector.
    pub iv: [u8; 16],
}

impl CipherKeys {
    /// Create a cipher key from a 16-byte shared secret (used as key and IV).
    pub fn from_shared_secret(secret: [u8; 16]) -> Self {
        Self {
            key: secret,
            iv: secret,
        }
    }
}

/// Reads framed packets from an async stream.
pub struct FrameReader<R> {
    stream: R,
    buf: BytesMut,
    compression: Option<u32>,
    cipher: Option<Cfb8>,
    decompressor: Decompress,
    chunk: Box<[u8]>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// Wrap an async reader.
    pub fn new(stream: R) -> Self {
        Self {
            stream,
            buf: BytesMut::with_capacity(READ_CHUNK),
            compression: None,
            cipher: None,
            decompressor: Decompress::new(true),
            chunk: vec![0u8; READ_CHUNK].into_boxed_slice(),
        }
    }

    /// Set the compression threshold announced by this side's peer.
    pub fn set_compression(&mut self, threshold: Option<u32>) {
        self.compression = threshold;
    }

    /// The active compression threshold.
    pub fn compression(&self) -> Option<u32> {
        self.compression
    }

    /// Enable AES-CFB8 decryption for all following frames.
    pub fn enable_encryption(&mut self, keys: &CipherKeys) {
        self.cipher = Some(Cfb8::new(&keys.key, &keys.iv, true));
    }

    /// Whether encryption is enabled.
    pub fn is_encrypted(&self) -> bool {
        self.cipher.is_some()
    }

    /// Push a byte back to the front of the stream buffer.
    ///
    /// Used for legacy (pre-1.7) pings, which are detected by peeking at the
    /// first byte before framing starts.
    pub fn push_back(&mut self, byte: u8) {
        let mut new = BytesMut::with_capacity(self.buf.len() + 1);
        new.extend_from_slice(&[byte]);
        new.extend_from_slice(&self.buf);
        self.buf = new;
    }

    /// Look at the next byte without consuming it.
    pub async fn peek_byte(&mut self) -> Result<u8> {
        if self.buf.is_empty() {
            self.fill(1).await?;
        }
        Ok(self.buf[0])
    }

    /// Read a single raw byte (used for legacy pings).
    pub async fn read_byte(&mut self) -> Result<u8> {
        if self.buf.is_empty() {
            self.fill(1).await?;
        }
        let byte = self.buf[0];
        self.buf.advance(1);
        Ok(byte)
    }

    /// Read the next packet.
    pub async fn read_packet(&mut self) -> Result<super::codec::Packet> {
        let (frame_len, len_size) = self.read_frame_len().await?;
        let total = len_size + frame_len;
        while self.buf.len() < total {
            self.fill(1).await?;
        }
        let frame = self.buf.split_to(total).freeze();
        let body = frame.slice(len_size..);

        let payload: Bytes = match self.compression {
            Some(_) => {
                let mut reader = Reader::new(&body);
                let data_len = reader.read_varint()?;
                if data_len < 0 {
                    return Err(ProtocolError::Violation(
                        "negative uncompressed length".into(),
                    ));
                }
                let offset = body.len() - reader.remaining();
                if data_len == 0 {
                    body.slice(offset..)
                } else {
                    let data_len = data_len as usize;
                    if data_len > MAX_UNCOMPRESSED_LEN {
                        return Err(ProtocolError::FrameTooLarge {
                            len: data_len,
                            max: MAX_UNCOMPRESSED_LEN,
                        });
                    }
                    let decompressed = self.decompress(&body[offset..], data_len)?;
                    Bytes::from(decompressed)
                }
            }
            None => body,
        };

        if payload.is_empty() {
            return Err(ProtocolError::Violation("empty packet".into()));
        }
        let (id, id_len) = read_varint(&payload)?;
        Ok(super::codec::Packet {
            id,
            data: payload.slice(id_len..),
        })
    }

    async fn read_frame_len(&mut self) -> Result<(usize, usize)> {
        loop {
            if let Some((len, size)) = try_varint(&self.buf)? {
                if len < 0 {
                    return Err(ProtocolError::Violation("negative frame length".into()));
                }
                let len = len as usize;
                if len > MAX_FRAME_LEN {
                    return Err(ProtocolError::FrameTooLarge {
                        len,
                        max: MAX_FRAME_LEN,
                    });
                }
                return Ok((len, size));
            }
            if self.buf.len() >= super::codec::MAX_VARINT_BYTES {
                return Err(ProtocolError::VarInt);
            }
            self.fill(1).await?;
        }
    }

    /// Read at least `min` more bytes into the internal buffer.
    async fn fill(&mut self, min: usize) -> Result<()> {
        let target = self.buf.len() + min;
        while self.buf.len() < target {
            let n = self.stream.read(&mut self.chunk).await?;
            if n == 0 {
                return Err(ProtocolError::Eof);
            }
            if let Some(cipher) = &mut self.cipher {
                cipher.apply_keystream(&mut self.chunk[..n]);
            }
            self.buf.extend_from_slice(&self.chunk[..n]);
        }
        Ok(())
    }

    fn decompress(&mut self, input: &[u8], expected: usize) -> Result<Vec<u8>> {
        // Reserve one extra byte so zlib can report the end of the stream even
        // when the payload is exactly `expected` bytes long.
        let limit = expected.saturating_add(1);
        let mut out = Vec::with_capacity(limit.min(64 * 1024 + 1));
        self.decompressor.reset(true);
        let mut input_pos = 0usize;
        for _ in 0..MAX_ZLIB_ITERATIONS {
            if out.len() >= limit {
                return Err(ProtocolError::Compression(
                    "decompressed data exceeds advertised size".into(),
                ));
            }
            let want = (limit - out.len()).min(256 * 1024);
            out.reserve_exact(want);
            let status = self
                .decompressor
                .decompress_vec(&input[input_pos..], &mut out, FlushDecompress::None)
                .map_err(|err| ProtocolError::Compression(err.to_string()))?;
            input_pos = self.decompressor.total_in() as usize;
            match status {
                Status::StreamEnd => break,
                Status::Ok | Status::BufError => {
                    if input_pos >= input.len() && out.len() < expected {
                        return Err(ProtocolError::Compression("truncated zlib stream".into()));
                    }
                }
            }
        }
        if out.len() != expected {
            return Err(ProtocolError::Compression(format!(
                "decompressed {} bytes, expected {}",
                out.len(),
                expected
            )));
        }
        Ok(out)
    }
}

/// Writes framed packets to an async stream.
pub struct FrameWriter<W> {
    stream: W,
    compression: Option<u32>,
    cipher: Option<Cfb8>,
    compressor: Compress,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    /// Wrap an async writer.
    pub fn new(stream: W) -> Self {
        Self {
            stream,
            compression: None,
            cipher: None,
            compressor: Compress::new(FlateLevel::default(), true),
        }
    }

    /// Set the compression threshold this side will use.
    pub fn set_compression(&mut self, threshold: Option<u32>) {
        self.compression = threshold;
    }

    /// The active compression threshold.
    pub fn compression(&self) -> Option<u32> {
        self.compression
    }

    /// Enable AES-CFB8 encryption for all following frames.
    pub fn enable_encryption(&mut self, keys: &CipherKeys) {
        self.cipher = Some(Cfb8::new(&keys.key, &keys.iv, false));
    }

    /// Whether encryption is enabled.
    pub fn is_encrypted(&self) -> bool {
        self.cipher.is_some()
    }

    /// Write a packet with the given id and payload.
    pub async fn write_packet(&mut self, id: i32, data: &[u8]) -> Result<()> {
        let mut payload = Vec::with_capacity(5 + data.len());
        write_varint(&mut payload, id);
        payload.extend_from_slice(data);
        self.write_payload(&payload).await
    }

    /// Write a pre-encoded payload (packet id included).
    pub async fn write_payload(&mut self, payload: &[u8]) -> Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 16);
        match self.compression {
            Some(threshold) if payload.len() >= threshold as usize => {
                let compressed = self.compress(payload)?;
                // The prefix is the length of the *uncompressed* payload.
                write_varint(&mut frame, payload.len() as i32);
                frame.extend_from_slice(&compressed);
            }
            Some(_) => {
                write_varint(&mut frame, 0);
                frame.extend_from_slice(payload);
            }
            None => frame.extend_from_slice(payload),
        }
        let mut out = Vec::with_capacity(frame.len() + 5);
        write_varint(&mut out, frame.len() as i32);
        out.extend_from_slice(&frame);
        if let Some(cipher) = &mut self.cipher {
            cipher.apply_keystream(&mut out);
        }
        self.stream.write_all(&out).await?;
        Ok(())
    }

    /// Write bytes without any framing (legacy ping responses).
    pub async fn write_raw(&mut self, data: &[u8]) -> Result<()> {
        self.stream.write_all(data).await?;
        Ok(())
    }

    /// Flush the underlying stream.
    pub async fn flush(&mut self) -> Result<()> {
        self.stream.flush().await?;
        Ok(())
    }

    /// Shut down the underlying stream.
    pub async fn shutdown(&mut self) -> Result<()> {
        self.stream.shutdown().await?;
        Ok(())
    }

    fn compress(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        // zlib's worst-case expansion is a fraction of a percent, so this
        // capacity is enough for small payloads; the loop grows geometrically
        // for the rare incompressible ones.
        let mut out = Vec::with_capacity(input.len() + input.len() / 100 + 64);
        self.compressor.reset();
        for _ in 0..MAX_ZLIB_ITERATIONS {
            if out.len() == out.capacity() {
                out.reserve(out.capacity().max(1024));
            }
            let consumed = self.compressor.total_in() as usize;
            let status = self
                .compressor
                .compress_vec(&input[consumed..], &mut out, FlushCompress::Finish)
                .map_err(|err| ProtocolError::Compression(err.to_string()))?;
            match status {
                Status::StreamEnd => {
                    if out.len() > MAX_UNCOMPRESSED_LEN {
                        return Err(ProtocolError::FrameTooLarge {
                            len: out.len(),
                            max: MAX_UNCOMPRESSED_LEN,
                        });
                    }
                    return Ok(out);
                }
                Status::Ok | Status::BufError => {}
            }
        }
        Err(ProtocolError::Compression(
            "compression did not terminate".into(),
        ))
    }
}

/// Read the first byte of a connection without consuming it.
pub async fn peek_first_byte<R: AsyncRead + Unpin>(reader: &mut FrameReader<R>) -> Result<u8> {
    let mut buf = [0u8; 1];
    let n = reader.stream.read(&mut buf).await?;
    if n == 0 {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
    }
    Ok(buf[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    async fn roundtrip(payload: &[u8], threshold: Option<u32>, encrypted: bool) {
        let (client, server) = duplex(128 * 1024);
        let keys = CipherKeys::from_shared_secret([7u8; 16]);
        let mut writer = FrameWriter::new(client);
        let mut reader = FrameReader::new(server);
        writer.set_compression(threshold);
        reader.set_compression(threshold);
        if encrypted {
            writer.enable_encryption(&keys);
            reader.enable_encryption(&keys);
        }
        let payload = payload.to_vec();
        let expected = payload.clone();
        let task = tokio::spawn(async move {
            writer.write_packet(0x2f, &payload).await.unwrap();
            writer.flush().await.unwrap();
        });
        let packet = reader.read_packet().await.unwrap();
        task.await.unwrap();
        assert_eq!(packet.id, 0x2f);
        assert_eq!(&packet.data[..], &expected[..]);
    }

    #[tokio::test]
    async fn uncompressed_roundtrip() {
        roundtrip(&vec![0xAB; 5000], None, false).await;
    }

    #[tokio::test]
    async fn compressed_small_packet_is_sent_uncompressed() {
        roundtrip(&[1, 2, 3], Some(256), false).await;
    }

    #[tokio::test]
    async fn compressed_large_packet() {
        let mut data = vec![0u8; 100_000];
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }
        roundtrip(&data, Some(256), false).await;
    }

    #[tokio::test]
    async fn compressed_300k_highly_compressible() {
        let payload: Vec<u8> = (0..300_000u32).map(|i| ((i * 7) % 256) as u8).collect();
        roundtrip(&payload, Some(8), false).await;
    }

    #[tokio::test]
    async fn encrypted_and_compressed_roundtrip() {
        roundtrip(&vec![0x5A; 70_000], Some(64), true).await;
    }

    #[tokio::test]
    async fn compression_threshold_zero_compresses_everything() {
        roundtrip(&[9u8; 4], Some(0), false).await;
    }
}
