//! The SQLite implementation of the Bridge's storage traits (ADR 0008).
//!
//! One database file holds all state. Its schema is migrated forward on open; a file written
//! by a newer Bridge (a schema this build does not know) is refused rather than guessed at.
//!
//! API keys are compared and looked up only by their SHA-256 fingerprint, and the key itself
//! goes in and out of the database only through [`seal`] and [`unseal`]. Both are the identity
//! for now; encrypting stored keys replaces just that pair.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use abs::ApiKey;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};
use store::{Error, LibraryUser, Session, SessionId, Store};

/// Schema migrations in order; the database's `user_version` counts those applied. Append
/// only: a released migration never changes.
const MIGRATIONS: &[&str] = &[include_str!("migrations/0001_sessions.sql")];

/// How API keys are stored: `seal` turns a key into what the database holds, `unseal` turns
/// that back into the key.
#[derive(Debug, Clone, Copy)]
struct Sealing {
    seal: fn(&ApiKey) -> Vec<u8>,
    unseal: fn(Vec<u8>) -> Result<ApiKey, String>,
}

/// Plain text, until stored keys are encrypted.
const PLAIN: Sealing = Sealing { seal, unseal };

fn seal(api_key: &ApiKey) -> Vec<u8> {
    api_key.expose().as_bytes().to_vec()
}

fn unseal(sealed: Vec<u8>) -> Result<ApiKey, String> {
    String::from_utf8(sealed)
        .map(ApiKey::new)
        .map_err(|_| "a stored API key is not UTF-8".to_owned())
}

/// What stored keys are compared by, instead of the (sealed) key itself.
fn fingerprint(api_key: &ApiKey) -> [u8; 32] {
    Sha256::digest(api_key.expose().as_bytes()).into()
}

/// A [`Store`] in one SQLite database file.
#[derive(Debug)]
pub struct SqliteStore {
    connection: Arc<Mutex<Connection>>,
    sealing: Sealing,
}

impl SqliteStore {
    /// Opens the database at `path`, creating it if it does not exist (its directory must),
    /// makes it readable by its owner only, since it holds API keys, and migrates its schema.
    pub fn open(path: &Path) -> Result<Self, Error> {
        Self::open_sealing(path, PLAIN)
    }

    fn open_sealing(path: &Path, sealing: Sealing) -> Result<Self, Error> {
        create_private(path).map_err(Error::new)?;
        let mut connection = Connection::open(path).map_err(Error::new)?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(Error::new)?;
        migrate(&mut connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            sealing,
        })
    }

    /// Runs `work` in one transaction on a blocking thread, committing if it succeeds.
    async fn transaction<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, Error> {
        let connection = Arc::clone(&self.connection);
        tokio::task::spawn_blocking(move || -> rusqlite::Result<T> {
            // A panic mid-transaction rolled it back when the transaction dropped, so the
            // connection is still consistent.
            let mut connection = connection.lock().unwrap_or_else(PoisonError::into_inner);
            let transaction = connection.transaction()?;
            let result = work(&transaction)?;
            transaction.commit()?;
            Ok(result)
        })
        .await
        .map_err(Error::new)?
        .map_err(Error::new)
    }
}

/// Creates the file if missing and restricts it to its owner, also when it already existed.
#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<()> {
    use std::fs::{OpenOptions, Permissions};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn create_private(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn migrate(connection: &mut Connection) -> Result<(), Error> {
    let known = MIGRATIONS.len();
    let transaction = connection.transaction().map_err(Error::new)?;
    let applied: i64 = transaction
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(Error::new)?;
    let applied = usize::try_from(applied)
        .ok()
        .filter(|&applied| applied <= known)
        .ok_or_else(|| {
            Error::new(format!(
                "the database has schema version {applied}, newer than this Bridge's {known}"
            ))
        })?;
    if applied == known {
        return Ok(());
    }
    for migration in &MIGRATIONS[applied..] {
        transaction.execute_batch(migration).map_err(Error::new)?;
    }
    transaction
        .pragma_update(None, "user_version", known as i64)
        .map_err(Error::new)?;
    transaction.commit().map_err(Error::new)
}

impl Store for SqliteStore {
    async fn open_session(
        &self,
        id: &SessionId,
        user: &LibraryUser,
        api_key: &ApiKey,
    ) -> Result<(), Error> {
        let (id, user) = (*id, user.clone());
        let (fingerprint, sealed) = (fingerprint(api_key), (self.sealing.seal)(api_key));
        self.transaction(move |tx| {
            // A key already stored for the user keeps its row (and sealed value); the no-op
            // update only makes `RETURNING` report the existing row.
            let api_key_id: i64 = tx
                .prepare_cached(
                    "INSERT INTO api_keys (library_server, user_id, key_fingerprint, sealed_key)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (library_server, user_id, key_fingerprint)
                     DO UPDATE SET key_fingerprint = excluded.key_fingerprint
                     RETURNING id",
                )?
                .query_row(
                    params![user.library_server, user.user_id, fingerprint, sealed],
                    |row| row.get(0),
                )?;
            tx.prepare_cached("INSERT INTO sessions (id, api_key_id) VALUES (?1, ?2)")?
                .execute(params![id.as_bytes(), api_key_id])?;
            Ok(())
        })
        .await
    }

    async fn session(&self, id: &SessionId) -> Result<Option<Session>, Error> {
        let (id, unseal) = (*id, self.sealing.unseal);
        self.transaction(move |tx| {
            tx.prepare_cached(
                "SELECT k.library_server, k.user_id, k.sealed_key
                 FROM sessions s JOIN api_keys k ON k.id = s.api_key_id
                 WHERE s.id = ?1",
            )?
            .query_row(params![id.as_bytes()], |row| {
                let api_key = unseal(row.get(2)?).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Blob,
                        error.into(),
                    )
                })?;
                Ok(Session {
                    user: LibraryUser {
                        library_server: row.get(0)?,
                        user_id: row.get(1)?,
                    },
                    api_key,
                })
            })
            .optional()
        })
        .await
    }

    async fn close_session(&self, id: &SessionId) -> Result<bool, Error> {
        let id = *id;
        self.transaction(move |tx| {
            let deleted = tx
                .prepare_cached("DELETE FROM sessions WHERE id = ?1")?
                .execute(params![id.as_bytes()])?;
            Ok(deleted > 0)
        })
        .await
    }

    async fn revoke_api_key(&self, user: &LibraryUser, api_key: &ApiKey) -> Result<(), Error> {
        let (user, fingerprint) = (user.clone(), fingerprint(api_key));
        self.transaction(move |tx| {
            tx.prepare_cached(
                "DELETE FROM api_keys
                 WHERE library_server = ?1 AND user_id = ?2 AND key_fingerprint = ?3",
            )?
            .execute(params![user.library_server, user.user_id, fingerprint])?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use store::contract::Backend;
    use tempfile::TempDir;

    use super::{MIGRATIONS, SqliteStore};

    /// A database file in a temporary directory, opened anew each time.
    struct TempDatabase {
        _dir: TempDir,
        path: PathBuf,
    }

    impl TempDatabase {
        fn new() -> Self {
            let dir = TempDir::new().expect("create temp dir");
            let path = dir.path().join("bridge.db");
            Self { _dir: dir, path }
        }
    }

    impl Backend for TempDatabase {
        type Store = SqliteStore;

        async fn open(&self) -> SqliteStore {
            SqliteStore::open(&self.path).expect("open database")
        }
    }

    store::store_contract_tests!(TempDatabase::new());

    /// The contract with a sealing that, like encryption, never stores the same key the same
    /// way twice: nothing may depend on comparing sealed keys.
    mod randomly_sealed {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        use abs::ApiKey;
        use store::contract::Backend;

        use super::TempDatabase;
        use crate::{Sealing, SqliteStore};

        const NONCE_LEN: usize = 16;

        fn seal(api_key: &ApiKey) -> Vec<u8> {
            static SEALED: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64;
            let mut sealed = Vec::with_capacity(NONCE_LEN + api_key.expose().len());
            sealed.extend_from_slice(&nanos.to_le_bytes());
            sealed.extend_from_slice(&SEALED.fetch_add(1, Ordering::Relaxed).to_le_bytes());
            sealed.extend_from_slice(api_key.expose().as_bytes());
            sealed
        }

        fn unseal(sealed: Vec<u8>) -> Result<ApiKey, String> {
            String::from_utf8(sealed[NONCE_LEN..].to_vec())
                .map(ApiKey::new)
                .map_err(|error| error.to_string())
        }

        struct RandomlySealed(TempDatabase);

        impl Backend for RandomlySealed {
            type Store = SqliteStore;

            async fn open(&self) -> SqliteStore {
                SqliteStore::open_sealing(&self.0.path, Sealing { seal, unseal })
                    .expect("open database")
            }
        }

        store::store_contract_tests!(RandomlySealed(TempDatabase::new()));
    }

    #[test]
    fn refuses_a_database_from_a_newer_bridge() {
        let database = TempDatabase::new();
        let newer = MIGRATIONS.len() as i64 + 1;
        rusqlite::Connection::open(&database.path)
            .unwrap()
            .pragma_update(None, "user_version", newer)
            .unwrap();

        let error = SqliteStore::open(&database.path).unwrap_err().to_string();
        assert!(
            error.contains(&format!("schema version {newer}")),
            "{error}"
        );
    }

    #[cfg(unix)]
    mod permissions {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        use super::TempDatabase;
        use crate::SqliteStore;

        fn mode(database: &TempDatabase) -> u32 {
            std::fs::metadata(&database.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        }

        #[test]
        fn only_the_owner_can_read_a_new_database() {
            let database = TempDatabase::new();
            SqliteStore::open(&database.path).unwrap();
            assert_eq!(mode(&database), 0o600);
        }

        #[test]
        fn an_existing_database_is_made_private() {
            let database = TempDatabase::new();
            std::fs::write(&database.path, b"").unwrap();
            std::fs::set_permissions(&database.path, Permissions::from_mode(0o644)).unwrap();

            SqliteStore::open(&database.path).unwrap();

            assert_eq!(mode(&database), 0o600);
        }
    }
}
