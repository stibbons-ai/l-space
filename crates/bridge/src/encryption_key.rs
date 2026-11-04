//! Loads the key the Bridge encrypts stored API keys with, from the environment rather than the
//! configuration file, which may sit on the same (backed-up) volume as the database.
//!
//! [`FILE_VAR`] names a file holding the key (a Docker secret, say) and wins over [`VALUE_VAR`],
//! the key itself; a variable set to the empty string counts as unset. The key is 32 bytes,
//! base64-encoded (`openssl rand -base64 32`); whitespace around it is ignored. Errors name the
//! variable and what is wrong, never the key, the file's contents or its path (an operator may
//! put the key itself in [`FILE_VAR`] by mistake).

use std::ffi::OsString;
use std::io::Read;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use store_sqlite::EncryptionKey;

const FILE_VAR: &str = "BRIDGE_ENCRYPTION_KEY_FILE";
const VALUE_VAR: &str = "BRIDGE_ENCRYPTION_KEY";

/// Ends every error message.
const HINT: &str = "generate a key with `openssl rand -base64 32`";

/// The most a key file may hold. A key with whitespace around it is far shorter; the cap keeps a
/// path like `/dev/zero` from hanging the Bridge.
const MAX_FILE_LEN: u64 = 1024;

/// Loads the key from the process environment. Once it has loaded, warns if [`VALUE_VAR`] was
/// ignored because [`FILE_VAR`] is set too.
pub fn load() -> Result<EncryptionKey, LoadError> {
    let (file, value) = (set_var(FILE_VAR), set_var(VALUE_VAR));
    let ignored_value = file.is_some() && value.is_some();
    let key = match (file, value) {
        (Some(path), _) => load_file(path)?,
        (None, Some(value)) => {
            decode(value.as_encoded_bytes()).map_err(LoadError::UnusableValue)?
        }
        (None, None) => return Err(LoadError::Missing),
    };
    if ignored_value {
        tracing::warn!("both {FILE_VAR} and {VALUE_VAR} are set; using {FILE_VAR}");
    }
    Ok(key)
}

/// The variable `name`, unless it is unset or empty.
fn set_var(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

fn load_file(path: OsString) -> Result<EncryptionKey, LoadError> {
    let mut contents = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_FILE_LEN + 1).read_to_end(&mut contents))
        .map_err(LoadError::Unreadable)?;
    if contents.len() as u64 > MAX_FILE_LEN {
        return Err(LoadError::UnusableFile(Problem::TooLong));
    }
    decode(&contents).map_err(LoadError::UnusableFile)
}

/// The key `encoded` holds, base64 with surrounding whitespace.
fn decode(encoded: &[u8]) -> Result<EncryptionKey, Problem> {
    let encoded = encoded.trim_ascii();
    if encoded.is_empty() {
        return Err(Problem::Empty);
    }
    // The decoder's own error would quote the offending character, a piece of the key.
    let bytes = STANDARD.decode(encoded).map_err(|_| Problem::NotBase64)?;
    let bytes = <[u8; EncryptionKey::LEN]>::try_from(bytes)
        .map_err(|bytes| Problem::WrongLength(bytes.len()))?;
    Ok(EncryptionKey::new(bytes))
}

/// Why there is no usable key.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("neither {FILE_VAR} nor {VALUE_VAR} is set (or both are empty); {HINT}")]
    Missing,
    #[error("{FILE_VAR}: cannot read the file it names: {0}; {HINT}")]
    Unreadable(std::io::Error),
    #[error("{FILE_VAR}: the file it names {0}; {HINT}")]
    UnusableFile(Problem),
    #[error("{VALUE_VAR} {0}; {HINT}")]
    UnusableValue(Problem),
}

#[derive(Debug, thiserror::Error)]
pub enum Problem {
    #[error("is empty")]
    Empty,
    #[error("is longer than {MAX_FILE_LEN} bytes, too long for a key")]
    TooLong,
    #[error("is not base64")]
    NotBase64,
    #[error("decodes to {0} bytes, not {len}", len = EncryptionKey::LEN)]
    WrongLength(usize),
}
