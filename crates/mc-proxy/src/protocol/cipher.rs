//! AES-128 in CFB-8 mode.
//!
//! Minecraft encrypts the post-login byte stream with AES/CFB8 using the
//! 16-byte shared secret as both key and IV. The `cfb8` crate only exposes
//! one-shot `AsyncStreamCipher` adapters (which consume the cipher), so the
//! mode is implemented here on top of AES-128's block encryptor. It is a
//! 20-line construction and is verified against the CFB-8 test vector from
//! the `cfb8` crate documentation.

use aes::cipher::{BlockEncrypt, KeyInit};
use aes::Aes128;

/// AES-128-CFB8 stream cipher state.
pub struct Cfb8 {
    cipher: Aes128,
    register: [u8; 16],
    decrypt: bool,
}

impl Cfb8 {
    /// Create a cipher from a 16-byte key and IV.
    pub fn new(key: &[u8; 16], iv: &[u8; 16], decrypt: bool) -> Self {
        Self {
            cipher: Aes128::new(key.into()),
            register: *iv,
            decrypt,
        }
    }

    /// Encrypt or decrypt `data` in place, advancing the feedback register.
    pub fn apply_keystream(&mut self, data: &mut [u8]) {
        for byte in data.iter_mut() {
            let mut block = self.register.into();
            self.cipher.encrypt_block(&mut block);
            let keystream = block[0];
            let input = *byte;
            *byte = input ^ keystream;
            // CFB-8: feed back one byte, then shift the register left by one.
            let feedback = if self.decrypt { input } else { *byte };
            self.register.copy_within(1.., 0);
            self.register[15] = feedback;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_reference_cfb8_vector() {
        // Test vector from the cfb8 crate documentation (AES-128-CFB8).
        let key = [0x42u8; 16];
        let iv = [0x24u8; 16];
        let plaintext = *b"hello world! this is my plaintext.";
        let expected = [
            0x33, 0xb3, 0x56, 0xce, 0x91, 0x84, 0x29, 0x0c, 0x4c, 0x8f, 0xac, 0xc1, 0xc0, 0xb1,
            0xf9, 0x18, 0xd5, 0x47, 0x5a, 0xeb, 0x75, 0xb8, 0x8c, 0x16, 0x1c, 0xa6, 0x5b, 0xdf,
            0x05, 0xc7, 0x13, 0x7f, 0xf4, 0xb0,
        ];
        let mut buffer = plaintext;
        Cfb8::new(&key, &iv, false).apply_keystream(&mut buffer);
        assert_eq!(buffer, expected);

        Cfb8::new(&key, &iv, true).apply_keystream(&mut buffer);
        assert_eq!(buffer, plaintext);
    }

    #[test]
    fn streaming_in_chunks_matches_one_shot() {
        let key = [1u8; 16];
        let iv = [2u8; 16];
        let data: Vec<u8> = (0..1000).map(|i| (i % 251) as u8).collect();

        let mut one_shot = data.clone();
        Cfb8::new(&key, &iv, false).apply_keystream(&mut one_shot);

        let mut chunked = data.clone();
        let mut cipher = Cfb8::new(&key, &iv, false);
        for chunk in chunked.chunks_mut(7) {
            cipher.apply_keystream(chunk);
        }
        assert_eq!(one_shot, chunked);
    }
}
