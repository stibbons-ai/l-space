//! Loads the key the Bridge encrypts stored API keys with, from the environment rather than the
//! configuration file, which may sit on the same (backed-up) volume as the database.
//!
//! [`FILE_VAR`] names a file holding the key (a Docker secret, say) and wins over [`VALUE_VAR`],
//! the key itself. The key is 32 bytes, base64-encoded (`openssl rand -base64 32`); whitespace
//! around it is ignored. Errors name the variable (and the file) and what is wrong, never the
//! key or the file's contents.

use std::fmt;
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use store_sqlite::EncryptionKey;

pub const FILE_VAR: &str = "BRIDGE_ENCRYPTION_KEY_FILE";
pub const VALUE_VAR: &str = "BRIDGE_ENCRYPTION_KEY";

/// Ends every error message.
const HINT: &str = "generate a key with `openssl rand -base64 32`";

/// The key, and whether [`VALUE_VAR`] was ignored because [`FILE_VAR`] was set too.
#[derive(Debug)]
pub struct Loaded {
    pub key: EncryptionKey,
    pub ignored_value_var: bool,
}

/// Loads the key from the process environment.
pub fn load() -> Result<Loaded, LoadError> {
    let (file, value) = (std::env::var_os(FILE_VAR), std::env::var_os(VALUE_VAR));
    let ignored_value_var = file.is_some() && value.is_some();
    let key = match (file, value) {
        (Some(path), _) => load_file(PathBuf::from(path))?,
        (None, Some(value)) => {
            decode(value.as_encoded_bytes()).map_err(|problem| LoadError::Unusable {
                origin: Origin::Value,
                problem,
            })?
        }
        (None, None) => return Err(LoadError::Missing),
    };
    Ok(Loaded {
        key,
        ignored_value_var,
    })
}

fn load_file(path: PathBuf) -> Result<EncryptionKey, LoadError> {
    match std::fs::read(&path) {
        Ok(contents) => decode(&contents).map_err(|problem| LoadError::Unusable {
            origin: Origin::File(path),
            problem,
        }),
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
    #[error("{origin} {problem}; {HINT}")]
    Unusable { origin: Origin, problem: Problem },
}

/// Where an unusable key came from.
#[derive(Debug)]
pub enum Origin {
    File(PathBuf),
    Value,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => write!(f, "{FILE_VAR}: the file \"{}\"", path.display()),
            Self::Value => f.write_str(VALUE_VAR),
        }
    }
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
