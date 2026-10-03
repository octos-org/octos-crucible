//! Encryption for everything the platform stores, plus the password zip
//! handed back to submitters.
//!
//! Envelope scheme: [age](https://age-encryption.org/v1) with an X25519
//! recipient. age draws a fresh random file key per file, wraps it to the
//! public key (X25519 + HKDF-SHA256 + ChaCha20-Poly1305) and encrypts the
//! payload with ChaCha20-Poly1305 in 64 KiB authenticated chunks. Chosen
//! over a hand-rolled X25519/HKDF/AEAD construction because the format is
//! specified, audited and has independent implementations: the admin can
//! decrypt a blob with the stock `age` CLI, and the browser side (step 4)
//! can use an existing age library instead of re-implementing ours.
//!
//! A sealed file carries a one-line [`Envelope`] header naming the key id
//! (fingerprint of the public key), so blobs stay decryptable after a key
//! rotation.

mod zipper;

use std::fmt;
use std::str::FromStr;

use age::secrecy::ExposeSecret;
use crucible_core::envelope::{Envelope, EnvelopeError};
use sha2::{Digest, Sha256};

pub use zipper::{ZipError, read_password_zip, write_password_zip};

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("invalid public key (expected an age1... X25519 recipient)")]
    PublicKey,
    #[error("invalid private key (expected an AGE-SECRET-KEY-1... identity)")]
    PrivateKey,
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
    #[error("sealed with key {0}, which is not among the given private keys")]
    UnknownKey(String),
    #[error("encryption failed")]
    Encrypt,
    /// Wrong key, truncated or tampered ciphertext: deliberately one error.
    #[error("decryption failed (wrong key or corrupted data)")]
    Decrypt,
}

/// Fingerprint of a public key: the first 16 hex chars of SHA-256 over its
/// `age1...` encoding. Short enough to read, long enough not to collide
/// among the handful of keys a rotation history produces.
pub fn key_id(recipient: &str) -> String {
    hex::encode(&Sha256::digest(recipient.as_bytes())[..8])
}

#[derive(Clone)]
pub struct PublicKey(age::x25519::Recipient);

impl PublicKey {
    pub fn key_id(&self) -> String {
        key_id(&self.0.to_string())
    }
}

impl FromStr for PublicKey {
    type Err = CryptoError;
    fn from_str(s: &str) -> Result<Self, CryptoError> {
        s.trim()
            .parse()
            .map(PublicKey)
            .map_err(|_| CryptoError::PublicKey)
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

pub struct PrivateKey(age::x25519::Identity);

impl PrivateKey {
    pub fn generate() -> Self {
        PrivateKey(age::x25519::Identity::generate())
    }

    pub fn public(&self) -> PublicKey {
        PublicKey(self.0.to_public())
    }

    /// The `AGE-SECRET-KEY-1...` string, for storing in a secret manager.
    pub fn to_secret_string(&self) -> String {
        self.0.to_string().expose_secret().to_owned()
    }

    /// Parse an identity; accepts an age key file (comment lines allowed).
    pub fn parse(s: &str) -> Result<Self, CryptoError> {
        s.lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .and_then(|l| l.parse().ok())
            .map(PrivateKey)
            .ok_or(CryptoError::PrivateKey)
    }
}

impl fmt::Debug for PrivateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PrivateKey({})", self.public().key_id())
    }
}

/// Encrypt `plaintext` to `key`: envelope header + age ciphertext.
pub fn seal(key: &PublicKey, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mut out = Envelope::new(key.key_id()).header_line();
    out.extend(age::encrypt(&key.0, plaintext).map_err(|_| CryptoError::Encrypt)?);
    Ok(out)
}

/// Decrypt a sealed file with whichever of `keys` it was sealed to.
pub fn open(keys: &[PrivateKey], sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let (env, ciphertext) = Envelope::split(sealed)?;
    let key = keys
        .iter()
        .find(|k| k.public().key_id() == env.key_id)
        .ok_or_else(|| CryptoError::UnknownKey(env.key_id.clone()))?;
    age::decrypt(&key.0, ciphertext).map_err(|_| CryptoError::Decrypt)
}

/// Key id recorded in a sealed file, without decrypting it.
pub fn sealed_key_id(sealed: &[u8]) -> Result<String, CryptoError> {
    Ok(Envelope::split(sealed)?.0.key_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: &[u8] =
        b"{\"api_key\":\"sk-user-SECRET-abc\",\"endpoint\":\"https://api.example.com/v1\"}";

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn round_trip_and_no_plaintext() {
        let sk = PrivateKey::generate();
        let sealed = seal(&sk.public(), PLAIN).unwrap();
        assert!(!contains(&sealed, b"SECRET"));
        assert!(!contains(&sealed, b"example.com"));
        assert_eq!(sealed_key_id(&sealed).unwrap(), sk.public().key_id());
        assert_eq!(open(&[sk], &sealed).unwrap(), PLAIN);
    }

    #[test]
    fn fresh_file_key_per_seal() {
        let sk = PrivateKey::generate();
        let a = seal(&sk.public(), PLAIN).unwrap();
        let b = seal(&sk.public(), PLAIN).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn tamper_rejected() {
        let sk = PrivateKey::generate();
        let sealed = seal(&sk.public(), PLAIN).unwrap();
        let header_len = sealed.iter().position(|&b| b == b'\n').unwrap() + 1;
        // Flip one bit at a few positions: in the age header, in the
        // payload, and in the final tag.
        for pos in [header_len + 30, sealed.len() - 40, sealed.len() - 1] {
            let mut bad = sealed.clone();
            bad[pos] ^= 1;
            assert!(open(std::slice::from_ref(&sk), &bad).is_err(), "pos {pos}");
        }
        // Truncation.
        assert!(open(std::slice::from_ref(&sk), &sealed[..sealed.len() - 1]).is_err());
    }

    #[test]
    fn wrong_key_rejected() {
        let sk = PrivateKey::generate();
        let other = PrivateKey::generate();
        let sealed = seal(&sk.public(), PLAIN).unwrap();
        assert!(matches!(
            open(std::slice::from_ref(&other), &sealed),
            Err(CryptoError::UnknownKey(_))
        ));
        // Header relabelled to the other key's id: age itself refuses.
        let mut relabelled = Envelope::new(other.public().key_id()).header_line();
        let header_len = sealed.iter().position(|&b| b == b'\n').unwrap() + 1;
        relabelled.extend_from_slice(&sealed[header_len..]);
        assert!(matches!(
            open(&[other], &relabelled),
            Err(CryptoError::Decrypt)
        ));
    }

    #[test]
    fn rotation_picks_matching_key() {
        let old = PrivateKey::generate();
        let new = PrivateKey::generate();
        let sealed_old = seal(&old.public(), b"old").unwrap();
        let sealed_new = seal(&new.public(), b"new").unwrap();
        let keys = [new, old];
        assert_eq!(open(&keys, &sealed_old).unwrap(), b"old");
        assert_eq!(open(&keys, &sealed_new).unwrap(), b"new");
    }

    #[test]
    fn key_parsing() {
        let sk = PrivateKey::generate();
        let file = format!(
            "# created: now\n# public key: {}\n{}\n",
            sk.public(),
            sk.to_secret_string()
        );
        let back = PrivateKey::parse(&file).unwrap();
        assert_eq!(back.public().key_id(), sk.public().key_id());
        let pk: PublicKey = sk.public().to_string().parse().unwrap();
        assert_eq!(pk.key_id(), sk.public().key_id());
        assert_eq!(pk.key_id().len(), 16);
        assert!("age1nope".parse::<PublicKey>().is_err());
        assert!(PrivateKey::parse("nope").is_err());
        assert!(!format!("{sk:?}").contains("SECRET"));
    }
}
