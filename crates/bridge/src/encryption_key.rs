//! Loads the key the Bridge encrypts stored API keys with, from the environment rather than the
//! configuration file, which may sit on the same (backed-up) volume as the database.
//!
//! [`FILE_VAR`] names a file holding the key (a Docker secret, say) and wins over [`VALUE_VAR`],
//! the key itself. The key is 32 bytes, base64-encoded (`openssl rand -base64 32`); whitespace
//! around it is ignored. Errors name the variable (and the file) and what is wrong, never the
//! key or the file's contents.

use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use store_sqlite::EncryptionKey;

const FILE_VAR: &str = "BRIDGE_ENCRYPTION_KEY_FILE";
const VALUE_VAR: &str = "BRIDGE_ENCRYPTION_KEY";

/// Ends every error message.
const HINT: &str = "generate a key with `openssl rand -base64 32`";

/// Loads the key from the process environment, warning if [`VALUE_VAR`] is ignored because
/// [`FILE_VAR`] is set too.
pub fn load() -> Result<EncryptionKey, LoadError> {
    let (file, value) = (std::env::var_os(FILE_VAR), std::env::var_os(VALUE_VAR));
    let ignored_value = file.is_some() && value.is_some();
    let key = match (file, value) {
        (Some(path), _) => load_file(PathBuf::from(path))?,
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

fn load_file(path: PathBuf) -> Result<EncryptionKey, LoadError> {
    match std::fs::read(&path) {
        Ok(contents) => {
            decode(&contents).map_err(|problem| LoadError::UnusableFile { path, problem })
        }
        Err(error) => Err(LoadError::Unreadable { path, error }),
    }
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
    #[error("neither {FILE_VAR} nor {VALUE_VAR} is set; {HINT}")]
    Missing,
    #[error("{FILE_VAR}: cannot read \"{}\": {error}; {HINT}", path.display())]
    Unreadable {
        path: PathBuf,
        error: std::io::Error,
    },
    #[error("{FILE_VAR}: the file \"{}\" {problem}; {HINT}", path.display())]
    UnusableFile { path: PathBuf, problem: Problem },
    #[error("{VALUE_VAR} {0}; {HINT}")]
    UnusableValue(Problem),
}

#[derive(Debug, thiserror::Error)]
pub enum Problem {
    #[error("is empty")]
    Empty,
    #[error("is not base64")]
    NotBase64,
    #[error("decodes to {0} bytes, not {len}", len = EncryptionKey::LEN)]
    WrongLength(usize),
}
