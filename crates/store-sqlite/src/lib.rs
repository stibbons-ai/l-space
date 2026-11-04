//! The SQLite implementation of the Bridge's storage traits (ADR 0008).
//!
//! One database file holds all state. Its schema is migrated forward on open; a file written
//! by a newer Bridge (a schema this build does not know) is refused rather than guessed at.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use abs::ApiKey;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use store::{Error, LibraryUser, Session, SessionId, Store};

/// Schema migrations in order; the database's `user_version` counts those applied. Append
/// only: a released migration never changes.
const MIGRATIONS: &[&str] = &[include_str!("migrations/0001_sessions.sql")];

/// A [`Store`] in one SQLite database file.
#[derive(Debug)]
pub struct SqliteStore {
    connection: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    /// Opens the database at `path`, creating it (readable by its owner only, since it holds
    /// API keys) if it does not exist, and migrates its schema.
    pub fn open(path: &Path) -> Result<Self, Error> {
        create_private(path).map_err(Error::new)?;
        let mut connection = Connection::open(path).map_err(Error::new)?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(Error::new)?;
        migrate(&mut connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
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

#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map(drop)
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
        let (id, user, api_key) = (*id, user.clone(), api_key.clone());
        self.transaction(move |tx| {
            // A different key is replaced, ending the sessions opened with it.
            tx.prepare_cached(
                "DELETE FROM api_keys WHERE library_server = ?1 AND user_id = ?2 AND api_key <> ?3",
            )?
            .execute(params![user.library_server, user.user_id, api_key.expose()])?;
            let api_key_id: i64 = tx
                .prepare_cached(
                    "INSERT INTO api_keys (library_server, user_id, api_key) VALUES (?1, ?2, ?3)
                     ON CONFLICT (library_server, user_id) DO UPDATE SET api_key = excluded.api_key
                     RETURNING id",
                )?
                .query_row(
                    params![user.library_server, user.user_id, api_key.expose()],
                    |row| row.get(0),
                )?;
            tx.prepare_cached("INSERT INTO sessions (id, api_key_id) VALUES (?1, ?2)")?
                .execute(params![id.as_bytes(), api_key_id])?;
            Ok(())
        })
        .await
    }

    async fn session(&self, id: &SessionId) -> Result<Option<Session>, Error> {
        let id = *id;
        self.transaction(move |tx| {
            tx.prepare_cached(
                "SELECT k.library_server, k.user_id, k.api_key
                 FROM sessions s JOIN api_keys k ON k.id = s.api_key_id
                 WHERE s.id = ?1",
            )?
            .query_row(params![id.as_bytes()], |row| {
                Ok(Session {
                    user: LibraryUser {
                        library_server: row.get(0)?,
                        user_id: row.get(1)?,
                    },
                    api_key: ApiKey::new(row.get::<_, String>(2)?),
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
        let (user, api_key) = (user.clone(), api_key.clone());
        self.transaction(move |tx| {
            tx.prepare_cached(
                "DELETE FROM api_keys WHERE library_server = ?1 AND user_id = ?2 AND api_key = ?3",
            )?
            .execute(params![user.library_server, user.user_id, api_key.expose()])?;
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
    #[test]
    fn only_the_owner_can_read_the_database() {
        use std::os::unix::fs::PermissionsExt;

        let database = TempDatabase::new();
        SqliteStore::open(&database.path).unwrap();

        let mode = std::fs::metadata(&database.path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
