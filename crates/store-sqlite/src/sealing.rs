//! How API keys are stored: encrypted, and compared only by a keyed fingerprint.
//!
//! The operator's [`EncryptionKey`] never touches the data directly. HKDF-SHA256 derives three
//! values from it, each for one purpose:
//!
//! - the XChaCha20-Poly1305 key that seals (encrypts and authenticates) each stored API key,
//!   with a fresh random nonce per seal, bound to its row (Library Server, user id,
//!   fingerprint) as associated data, so a sealed key moved to another row doesn't open;
//! - the HMAC-SHA256 key for fingerprints, what stored keys are compared and looked up by, so
//!   a copy of the database alone can't confirm a guessed API key;
//! - the key id stored with every row, naming the encryption key and this scheme, so rows
//!   sealed under another key can be counted without trying to open them.

use std::fmt;

use abs::ApiKey;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use store::LibraryUser;

/// The secret the Bridge's stored API keys are encrypted with: 32 bytes the operator
/// provides. It never prints.
pub struct EncryptionKey([u8; EncryptionKey::LEN]);

impl EncryptionKey {
    pub const LEN: usize = 32;

    pub fn new(bytes: [u8; Self::LEN]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for EncryptionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EncryptionKey(REDACTED)")
    }
}

const NONCE_LEN: usize = 24;

/// The HKDF `info` of each derived value. Changing the scheme changes these, and with them the
/// key id.
const SEAL_INFO: &[u8] = b"l-space bridge v1: API key sealing";
const FINGERPRINT_INFO: &[u8] = b"l-space bridge v1: API key fingerprint";
const KEY_ID_INFO: &[u8] = b"l-space bridge v1: key id";

/// Identifies the encryption key and sealing scheme a row was sealed with.
pub type KeyId = [u8; 16];

/// What a stored key is compared and looked up by.
pub type Fingerprint = [u8; 32];

/// The row a sealed key belongs to: opening it anywhere else fails.
pub struct Row<'a> {
    pub library_server: &'a str,
    pub user_id: &'a str,
    pub fingerprint: &'a Fingerprint,
}

impl<'a> Row<'a> {
    /// The row of `user`'s key with `fingerprint`.
    pub fn of(user: &'a LibraryUser, fingerprint: &'a Fingerprint) -> Self {
        Self {
            library_server: &user.library_server,
            user_id: &user.user_id,
            fingerprint,
        }
    }

    /// The row as associated data, each variable-length field prefixed with its length so no
    /// two rows encode alike.
    fn associated_data(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(
            16 + self.library_server.len() + self.user_id.len() + self.fingerprint.len(),
        );
        for field in [self.library_server, self.user_id] {
            data.extend_from_slice(&(field.len() as u64).to_le_bytes());
            data.extend_from_slice(field.as_bytes());
        }
        data.extend_from_slice(self.fingerprint);
        data
    }
}

/// Seals and opens API keys, and fingerprints them, under one [`EncryptionKey`].
pub struct Sealing {
    cipher: XChaCha20Poly1305,
    fingerprinter: Hmac<Sha256>,
    key_id: KeyId,
}

impl fmt::Debug for Sealing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Sealing")
    }
}

impl Sealing {
    pub fn new(key: &EncryptionKey) -> Self {
        let derive = Hkdf::<Sha256>::new(None, &key.0);
        let mut seal_key = [0; 32];
        let mut fingerprint_key = [0; 32];
        let mut key_id = KeyId::default();
        for (info, okm) in [
            (SEAL_INFO, &mut seal_key[..]),
            (FINGERPRINT_INFO, &mut fingerprint_key[..]),
            (KEY_ID_INFO, &mut key_id[..]),
        ] {
            derive
                .expand(info, okm)
                .expect("HKDF-SHA256 yields up to 8160 bytes");
        }
        Self {
            cipher: XChaCha20Poly1305::new(&seal_key.into()),
            fingerprinter: <Hmac<Sha256> as KeyInit>::new_from_slice(&fingerprint_key)
                .expect("HMAC takes a key of any length"),
            key_id,
        }
    }

    pub fn key_id(&self) -> &KeyId {
        &self.key_id
    }

    pub fn fingerprint(&self, api_key: &ApiKey) -> Fingerprint {
        let mut mac = self.fingerprinter.clone();
        mac.update(api_key.expose().as_bytes());
        mac.finalize().into_bytes().into()
    }

    /// `api_key` encrypted for `row`: a random nonce, then the ciphertext and its tag.
    pub fn seal(&self, row: &Row<'_>, api_key: &ApiKey) -> Result<Vec<u8>, SealError> {
        let mut nonce = [0; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(SealError::NoRandomness)?;
        let ciphertext = self
            .cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: api_key.expose().as_bytes(),
                    aad: &row.associated_data(),
                },
            )
            .map_err(|_| SealError::Encrypt)?;
        let mut sealed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);
        Ok(sealed)
    }

    /// The key [`Sealing::seal`] sealed for `row`, or `None` if `sealed` doesn't open: it was
    /// sealed under another encryption key, for another row, or is damaged.
    pub fn open(&self, row: &Row<'_>, sealed: &[u8]) -> Option<ApiKey> {
        let (nonce, ciphertext) = sealed.split_first_chunk::<NONCE_LEN>()?;
        let nonce = XNonce::from(*nonce);
        let plaintext = self
            .cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: ciphertext,
                    aad: &row.associated_data(),
                },
            )
            .ok()?;
        String::from_utf8(plaintext).ok().map(ApiKey::new)
    }
}

/// Sealing an API key failed.
#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error("cannot seal the API key: no randomness for its nonce")]
    NoRandomness(#[source] getrandom::Error),
    /// The API key is too long to encrypt (about 256 GiB or more).
    #[error("cannot seal the API key: encryption failed")]
    Encrypt,
}

#[cfg(test)]
mod tests {
    use abs::ApiKey;

    use super::{EncryptionKey, Fingerprint, Row, Sealing};

    fn under(byte: u8) -> Sealing {
        Sealing::new(&EncryptionKey::new([byte; EncryptionKey::LEN]))
    }

    const FINGERPRINT: Fingerprint = [9; 32];

    fn row<'a>(library_server: &'a str, user_id: &'a str) -> Row<'a> {
        Row {
            library_server,
            user_id,
            fingerprint: &FINGERPRINT,
        }
    }

    #[test]
    fn a_sealed_key_opens_only_with_its_key_and_row() {
        let sealing = under(1);
        let alice = row("https://abs.lan", "alice-id");
        let key = ApiKey::new("alice-key");
        let sealed = sealing.seal(&alice, &key).unwrap();

        assert_eq!(sealing.open(&alice, &sealed), Some(key));
        assert_eq!(under(2).open(&alice, &sealed), None);
        assert_eq!(
            sealing.open(&row("https://abs.lan", "bob-id"), &sealed),
            None
        );
        assert_eq!(
            sealing.open(&row("https://other.lan", "alice-id"), &sealed),
            None
        );
        // Length prefixes keep a shifted boundary between the fields from matching.
        assert_eq!(
            sealing.open(&row("https://abs.la", "nalice-id"), &sealed),
            None
        );
        let other_fingerprint = Row {
            fingerprint: &[8; 32],
            ..alice
        };
        assert_eq!(sealing.open(&other_fingerprint, &sealed), None);
    }

    #[test]
    fn a_damaged_seal_does_not_open() {
        let sealing = under(1);
        let alice = row("https://abs.lan", "alice-id");
        let sealed = sealing.seal(&alice, &ApiKey::new("alice-key")).unwrap();

        for at in [0, 23, 24, sealed.len() - 1] {
            let mut damaged = sealed.clone();
            damaged[at] ^= 1;
            assert_eq!(sealing.open(&alice, &damaged), None, "flipped byte {at}");
        }
        for len in [0, 23, 24, sealed.len() - 1] {
            assert_eq!(sealing.open(&alice, &sealed[..len]), None, "cut to {len}");
        }
        assert_eq!(sealing.open(&alice, b"alice-key"), None, "plain text");
    }

    #[test]
    fn sealing_twice_never_repeats_and_hides_the_key() {
        let sealing = under(1);
        let alice = row("https://abs.lan", "alice-id");
        let key = ApiKey::new("alice-key");
        let first = sealing.seal(&alice, &key).unwrap();
        let second = sealing.seal(&alice, &key).unwrap();

        assert_ne!(first, second);
        for sealed in [first, second] {
            assert!(!sealed.windows(9).any(|window| window == b"alice-key"));
        }
    }

    #[test]
    fn fingerprints_and_key_ids_depend_on_the_encryption_key() {
        let key = ApiKey::new("alice-key");
        let (one, two) = (under(1), under(2));

        assert_eq!(one.fingerprint(&key), under(1).fingerprint(&key));
        assert_ne!(one.fingerprint(&key), two.fingerprint(&key));
        assert_ne!(
            one.fingerprint(&key),
            one.fingerprint(&ApiKey::new("bob-key"))
        );
        assert_eq!(one.key_id(), under(1).key_id());
        assert_ne!(one.key_id(), two.key_id());
    }

    #[test]
    fn debug_never_prints_key_material() {
        let key = EncryptionKey::new([0xAB; EncryptionKey::LEN]);
        assert_eq!(format!("{key:?}"), "EncryptionKey(REDACTED)");
        assert_eq!(format!("{:?}", Sealing::new(&key)), "Sealing");
    }
}
