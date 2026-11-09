//! Cryptography used by the online-mode login handshake.

use md5::Md5;
use rsa::pkcs8::EncodePublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPrivateKey, RsaPublicKey};

use sha1::{Digest, Sha1};
use uuid::Uuid;

/// Error type for cryptographic failures.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// RSA key generation or encoding failed.
    #[error("rsa error: {0}")]
    Rsa(#[from] rsa::Error),
    /// RSA public key encoding failed.
    #[error("rsa encoding error: {0}")]
    RsaEncoding(#[from] rsa::pkcs8::spki::Error),
    /// Decryption failed.
    #[error("decryption failed")]
    Decrypt,
    /// The system random number generator failed.
    #[error("random number generator failed: {0}")]
    Random(String),
}

/// The proxy's RSA key pair, generated once at startup.
///
/// Minecraft uses 1024-bit keys; the client encrypts the shared secret and the
/// verify token with PKCS#1 v1.5.
pub struct ServerKey {
    private: RsaPrivateKey,
    public_der: Vec<u8>,
}

impl ServerKey {
    /// Generate a fresh 1024-bit key pair.
    pub fn generate() -> Result<Self, CryptoError> {
        let mut rng = rand::rngs::OsRng;
        let private = RsaPrivateKey::new(&mut rng, 1024)?;
        let public = RsaPublicKey::from(&private);
        let public_der = public.to_public_key_der()?.as_bytes().to_vec();
        Ok(Self {
            private,
            public_der,
        })
    }

    /// The X.509 SubjectPublicKeyInfo DER sent in the Encryption Request.
    pub fn public_key_der(&self) -> &[u8] {
        &self.public_der
    }

    /// Decrypt a PKCS#1 v1.5 ciphertext.
    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        self.private
            .decrypt(Pkcs1v15Encrypt, ciphertext)
            .map_err(|_| CryptoError::Decrypt)
    }
}

/// Generate `len` cryptographically secure random bytes.
pub fn random_bytes(len: usize) -> Result<Vec<u8>, CryptoError> {
    let mut buf = vec![0u8; len];
    getrandom::fill(&mut buf).map_err(|err| CryptoError::Random(err.to_string()))?;
    Ok(buf)
}

/// Generate the 4-byte verify token used during encryption.
pub fn random_verify_token() -> Result<[u8; 4], CryptoError> {
    let mut token = [0u8; 4];
    getrandom::fill(&mut token).map_err(|err| CryptoError::Random(err.to_string()))?;
    Ok(token)
}

/// Compute Minecraft's "server hash" used for session server authentication.
///
/// This is the SHA-1 of `server_id + shared_secret + public_key` rendered as a
/// signed hexadecimal `BigInteger`, exactly like vanilla's
/// `ServerLoginPacketListenerImpl#createChallenge`.
pub fn minecraft_server_hash(
    server_id: &str,
    shared_secret: &[u8],
    public_key_der: &[u8],
) -> String {
    let mut hasher = Sha1::new();
    hasher.update(server_id.as_bytes());
    hasher.update(shared_secret);
    hasher.update(public_key_der);
    let digest = hasher.finalize();

    let mut bytes = digest.to_vec();
    let negative = bytes[0] & 0x80 != 0;
    if negative {
        // Two's complement negation for the BigInteger representation.
        for byte in bytes.iter_mut() {
            *byte = !*byte;
        }
        let mut carry = 1u16;
        for byte in bytes.iter_mut().rev() {
            let value = u16::from(*byte) + carry;
            *byte = value as u8;
            carry = value >> 8;
        }
    }
    let first_nonzero = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len());
    let hex: String = bytes[first_nonzero..]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let hex = if hex.is_empty() { "0".to_owned() } else { hex };
    if negative {
        format!("-{hex}")
    } else {
        hex
    }
}

/// Compute the offline-mode UUID for a player name.
///
/// Matches `UUID.nameUUIDFromBytes(("OfflinePlayer:" + name).asBytes())` from
/// the vanilla server.
pub fn offline_uuid(name: &str) -> Uuid {
    let digest = Md5::digest(format!("OfflinePlayer:{name}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest);
    bytes[6] = (bytes[6] & 0x0f) | 0x30;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs8::DecodePublicKey;

    #[test]
    fn offline_uuid_matches_vanilla() {
        // Values produced by java.util.UUID.nameUUIDFromBytes:
        // ("OfflinePlayer:" + name).getBytes(UTF_8)
        assert_eq!(
            offline_uuid("Notch").to_string(),
            "b50ad385-829d-3141-a216-7e7d7539ba7f"
        );
        assert_eq!(
            offline_uuid("jeb_").to_string(),
            "a762f560-4fce-3236-812a-b80efff0b62b"
        );
    }

    #[test]
    fn server_hash_is_signed_hex() {
        // Java: new BigInteger(digest).toString(16)
        let hash = minecraft_server_hash("", b"secret-secret-se", b"public-key");
        assert!(!hash.is_empty());
        assert!(hash
            .trim_start_matches('-')
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // Known-answer test against Java's BigInteger output.
        assert_eq!(
            minecraft_server_hash("", b"secret-secret-se", b"public-key"),
            "28aec1464f9065169c3f3f631f2d5d4935744d57"
        );
        assert_eq!(
            minecraft_server_hash("notch", b"1234567890123456", b"key"),
            "-32c473c230f4b865559332e8f1dc6d5c4dfcf733"
        );
    }

    #[test]
    fn rsa_roundtrip() {
        let key = ServerKey::generate().unwrap();
        assert!(!key.public_key_der().is_empty());
        let public = RsaPublicKey::from_public_key_der(key.public_key_der()).unwrap();
        let mut rng = rand::rngs::OsRng;
        let secret = [42u8; 16];
        let ciphertext = public.encrypt(&mut rng, Pkcs1v15Encrypt, &secret).unwrap();
        assert_eq!(key.decrypt(&ciphertext).unwrap(), secret);
    }
}
