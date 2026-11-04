//! The SQLite implementation of the Bridge's storage traits (ADR 0008).
//!
//! One database file holds all state. Its schema is migrated forward on open; a file written
//! by a newer Bridge (a schema this build does not know) is refused rather than guessed at.
//!
//! API keys are stored encrypted under the operator's [`EncryptionKey`], each bound to its row,
//! and compared and looked up only by a fingerprint keyed by it (see `sealing`). A key that no
//! longer decrypts (the Bridge runs with another encryption key, or the row was tampered with)
//! is removed with its sessions when one of them is looked up: the session is gone, and its
//! user logs in again. On open, the store removes keys sealed under another encryption key
//! that no session uses, and warns if any such keys still have sessions.

mod sealing;

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use abs::ApiKey;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use store::{Error, LibraryUser, Session, SessionId, Store};

pub use crate::sealing::EncryptionKey;
use crate::sealing::{Fingerprint, Row, Sealing};

/// Schema migrations in order; the database's `user_version` counts those applied. Append
/// only: a released migration never changes.
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_sessions.sql"),
    include_str!("migrations/0002_encrypted_api_keys.sql"),
];

/// A [`Store`] in one SQLite database file.
pub struct SqliteStore {
    connection: Arc<Mutex<Connection>>,
    sealing: Arc<Sealing>,
}

impl fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteStore").finish_non_exhaustive()
    }
}

impl SqliteStore {
    /// Opens the database at `path`, creating it if it does not exist (its directory must),
    /// on Unix makes it readable by its owner only, and migrates its schema. API keys are
    /// stored encrypted under `key`.
    pub fn open(path: &Path, key: &EncryptionKey) -> Result<Self, Error> {
        create_private(path)?;
        let mut connection = Connection::open(path).map_err(step("open the database"))?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(step("enable foreign keys"))?;
        // Deleted keys are overwritten rather than left in free pages.
        connection
            .pragma_update(None, "secure_delete", true)
            .map_err(step("enable secure delete"))?;
        let applied = migrate(&mut connection).map_err(step("migrate the schema"))?;
        if (1..FIRST_ENCRYPTED_SCHEMA).contains(&applied) {
            // The earlier Bridge stored keys in plain text and never set `secure_delete`, so
            // keys it deleted may linger in free pages; rebuilding the file drops them.
            connection
                .execute_batch("VACUUM")
                .map_err(step("drop plain-text keys from free pages"))?;
        }
        let sealing = Sealing::new(key);
        forget_keys_sealed_under_another_key(&connection, &sealing)
            .map_err(step("forget keys sealed under another encryption key"))?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            sealing: Arc::new(sealing),
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

/// Opening the database failed at `step`.
#[derive(Debug, thiserror::Error)]
#[error("{step}")]
struct OpenError {
    step: &'static str,
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
}

/// Wraps an error from `step` of opening the database, keeping it as the source.
fn step<E: Into<Box<dyn std::error::Error + Send + Sync>>>(
    step: &'static str,
) -> impl FnOnce(E) -> Error {
    move |source| {
        Error::new(OpenError {
            step,
            source: source.into(),
        })
    }
}

/// Creates the file if missing and restricts it to its owner, also when it already existed.
#[cfg(unix)]
fn create_private(path: &Path) -> Result<(), Error> {
    use std::fs::{OpenOptions, Permissions};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(step("create the database file"))?;
    file.set_permissions(Permissions::from_mode(0o600))
        .map_err(step("make the database owner-only"))
}

#[cfg(not(unix))]
fn create_private(_path: &Path) -> Result<(), Error> {
    Ok(())
}

/// The schema version from which API keys are stored encrypted.
const FIRST_ENCRYPTED_SCHEMA: usize = 2;

/// Migrates the schema to the newest, returning the version the database had.
fn migrate(connection: &mut Connection) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let known = MIGRATIONS.len();
    let transaction = connection.transaction()?;
    let applied: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let applied = usize::try_from(applied)
        .ok()
        .filter(|&applied| applied <= known)
        .ok_or_else(|| {
            format!("the database has schema version {applied}, newer than this Bridge's {known}")
        })?;
    if applied == known {
        return Ok(applied);
    }
    for migration in &MIGRATIONS[applied..] {
        transaction.execute_batch(migration)?;
    }
    transaction.pragma_update(None, "user_version", known as i64)?;
    transaction.commit()?;
    Ok(applied)
}

/// Removes keys sealed under another encryption key than `sealing`'s that no session uses
/// (nothing would ever look them up again), and warns once if others remain: their sessions
/// end at their next call.
fn forget_keys_sealed_under_another_key(
    connection: &Connection,
    sealing: &Sealing,
) -> rusqlite::Result<()> {
    connection.execute(
        "DELETE FROM api_keys
         WHERE key_id <> ?1
           AND NOT EXISTS (SELECT 1 FROM sessions WHERE sessions.api_key_id = api_keys.id)",
        params![sealing.key_id()],
    )?;
    let keys: i64 = connection.query_row(
        "SELECT count(*) FROM api_keys WHERE key_id <> ?1",
        params![sealing.key_id()],
        |row| row.get(0),
    )?;
    if keys > 0 {
        tracing::warn!(
            keys,
            "stored API keys were encrypted with another encryption key; their sessions end \
             at their next call, and their users must log in again"
        );
    }
    Ok(())
}

/// A session looked up in the database.
enum Lookup {
    Open(Session),
    /// The session's key does not open: it and its sessions were removed.
    Unreadable(LibraryUser),
    Unknown,
}

impl Store for SqliteStore {
    async fn open_session(
        &self,
        id: &SessionId,
        user: &LibraryUser,
        api_key: &ApiKey,
    ) -> Result<(), Error> {
        let (id, user) = (*id, user.clone());
        let fingerprint = self.sealing.fingerprint(api_key);
        let sealed = self
            .sealing
            .seal(&Row::of(&user, &fingerprint), api_key)
            .map_err(Error::new)?;
        let key_id = *self.sealing.key_id();
        self.transaction(move |tx| {
            // A key already stored for the user keeps its row; logging in re-seals it.
            let api_key_id: i64 = tx
                .prepare_cached(
                    "INSERT INTO api_keys
                         (library_server, user_id, key_fingerprint, sealed_key, key_id)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (library_server, user_id, key_fingerprint)
                     DO UPDATE SET sealed_key = excluded.sealed_key
                     RETURNING id",
                )?
                .query_row(
                    params![
                        user.library_server,
                        user.user_id,
                        fingerprint,
                        sealed,
                        key_id
                    ],
                    |row| row.get(0),
                )?;
            tx.prepare_cached("INSERT INTO sessions (id, api_key_id) VALUES (?1, ?2)")?
                .execute(params![id.as_bytes(), api_key_id])?;
            Ok(())
        })
        .await
    }

    async fn session(&self, id: &SessionId) -> Result<Option<Session>, Error> {
        let (id, sealing) = (*id, Arc::clone(&self.sealing));
        let lookup = self
            .transaction(move |tx| {
                let Some((api_key_id, user, fingerprint, sealed)) = tx
                    .prepare_cached(
                        "SELECT k.id, k.library_server, k.user_id, k.key_fingerprint, k.sealed_key
                         FROM sessions s JOIN api_keys k ON k.id = s.api_key_id
                         WHERE s.id = ?1",
                    )?
                    .query_row(params![id.as_bytes()], |row| {
                        let user = LibraryUser {
                            library_server: row.get(1)?,
                            user_id: row.get(2)?,
                        };
                        Ok((
                            row.get::<_, i64>(0)?,
                            user,
                            row.get::<_, Fingerprint>(3)?,
                            row.get::<_, Vec<u8>>(4)?,
                        ))
                    })
                    .optional()?
                else {
                    return Ok(Lookup::Unknown);
                };
                if let Some(api_key) = sealing.open(&Row::of(&user, &fingerprint), &sealed) {
                    return Ok(Lookup::Open(Session { user, api_key }));
                }
                tx.prepare_cached("DELETE FROM api_keys WHERE id = ?1")?
                    .execute(params![api_key_id])?;
                Ok(Lookup::Unreadable(user))
            })
            .await?;
        Ok(match lookup {
            Lookup::Open(session) => Some(session),
            Lookup::Unreadable(user) => {
                tracing::warn!(
                    library_server = user.library_server,
                    user_id = user.user_id,
                    "a stored API key does not decrypt with the current encryption key (or was \
                     tampered with); it was removed and the sessions opened with it ended"
                );
                None
            }
            Lookup::Unknown => None,
        })
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

    async fn revoke_api_key(&self, user: &LibraryUser, api_key: &ApiKey) -> Result<bool, Error> {
        let (user, fingerprint) = (user.clone(), self.sealing.fingerprint(api_key));
        self.transaction(move |tx| {
            let deleted = tx
                .prepare_cached(
                    "DELETE FROM api_keys
                     WHERE library_server = ?1 AND user_id = ?2 AND key_fingerprint = ?3",
                )?
                .execute(params![user.library_server, user.user_id, fingerprint])?;
            Ok(deleted > 0)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use abs::ApiKey;
    use rusqlite::params;
    use store::contract::Backend;
    use store::{LibraryUser, SessionId, Store};
    use tempfile::TempDir;

    use super::{EncryptionKey, MIGRATIONS, SqliteStore};

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

        /// Opens the store with encryption key number `n`.
        fn open_with(&self, n: u8) -> SqliteStore {
            SqliteStore::open(&self.path, &EncryptionKey::new([n; EncryptionKey::LEN]))
                .expect("open database")
        }

        fn raw(&self) -> rusqlite::Connection {
            rusqlite::Connection::open(&self.path).unwrap()
        }

        /// The sealed key stored for user `user_id`, who has exactly one.
        fn sealed_key(&self, user_id: &str) -> Vec<u8> {
            self.raw()
                .query_row(
                    "SELECT sealed_key FROM api_keys WHERE user_id = ?1",
                    params![user_id],
                    |row| row.get(0),
                )
                .unwrap()
        }

        fn set_sealed_key(&self, user_id: &str, sealed: &[u8]) {
            self.raw()
                .execute(
                    "UPDATE api_keys SET sealed_key = ?2 WHERE user_id = ?1",
                    params![user_id, sealed],
                )
                .unwrap();
        }
    }

    impl Backend for TempDatabase {
        type Store = SqliteStore;

        async fn open(&self) -> SqliteStore {
            self.open_with(1)
        }
    }

    store::store_contract_tests!(TempDatabase::new());

    fn alice() -> LibraryUser {
        LibraryUser {
            library_server: "https://abs.lan".into(),
            user_id: "alice-id".into(),
        }
    }

    fn bob() -> LibraryUser {
        LibraryUser {
            user_id: "bob-id".into(),
            ..alice()
        }
    }

    fn token(n: u8) -> SessionId {
        SessionId::of_token(&format!("token-{n}"))
    }

    async fn login(store: &SqliteStore, n: u8, user: &LibraryUser, key: &str) {
        store
            .open_session(&token(n), user, &ApiKey::new(key))
            .await
            .unwrap();
    }

    async fn api_key(store: &SqliteStore, n: u8) -> Option<ApiKey> {
        store
            .session(&token(n))
            .await
            .expect("an unreadable key is no error")
            .map(|session| session.api_key)
    }

    /// Every byte the database file holds.
    fn file_bytes(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[tokio::test]
    async fn a_key_used_twice_is_stored_once() {
        let database = TempDatabase::new();
        let store = database.open_with(1);
        login(&store, 1, &alice(), "alice-key").await;
        login(&store, 2, &alice(), "alice-key").await;

        let stored: i64 = database
            .raw()
            .query_row("SELECT count(*) FROM api_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored, 1);
    }

    #[tokio::test]
    async fn the_file_holds_no_plain_key() {
        let database = TempDatabase::new();
        let store = database.open_with(1);
        login(&store, 1, &alice(), "alice-secret-key").await;
        drop(store);

        assert!(!contains(&file_bytes(&database.path), b"alice-secret-key"));
    }

    #[tokio::test]
    async fn another_encryption_key_ends_the_session_and_a_new_login_works() {
        let database = TempDatabase::new();
        login(&database.open_with(1), 1, &alice(), "alice-key").await;

        let other = database.open_with(2);
        assert_eq!(api_key(&other, 1).await, None);
        login(&other, 2, &alice(), "alice-key").await;
        assert_eq!(api_key(&other, 2).await, Some(ApiKey::new("alice-key")));

        // The unreadable key was removed, not just hidden: the original encryption key no
        // longer finds the session either.
        assert_eq!(api_key(&database.open_with(1), 1).await, None);
    }

    #[tokio::test]
    async fn a_tampered_key_ends_its_sessions_only() {
        let database = TempDatabase::new();
        let store = database.open_with(1);
        login(&store, 1, &alice(), "alice-key").await;
        login(&store, 2, &alice(), "alice-key").await;
        login(&store, 3, &bob(), "bob-key").await;
        let mut sealed = database.sealed_key("alice-id");
        sealed[30] ^= 1;
        database.set_sealed_key("alice-id", &sealed);

        assert_eq!(api_key(&store, 1).await, None);
        assert_eq!(api_key(&store, 2).await, None);
        assert_eq!(api_key(&store, 3).await, Some(ApiKey::new("bob-key")));
    }

    /// A sealed key moved to another row (another user, say) doesn't open there.
    #[tokio::test]
    async fn swapped_keys_open_for_neither_row() {
        let database = TempDatabase::new();
        let store = database.open_with(1);
        login(&store, 1, &alice(), "alice-key").await;
        login(&store, 2, &bob(), "bob-key").await;
        let (alices, bobs) = (
            database.sealed_key("alice-id"),
            database.sealed_key("bob-id"),
        );
        database.set_sealed_key("alice-id", &bobs);
        database.set_sealed_key("bob-id", &alices);

        assert_eq!(api_key(&store, 1).await, None);
        assert_eq!(api_key(&store, 2).await, None);
    }

    /// Keys sealed under another encryption key that no session uses are removed on open;
    /// those with sessions stay until a session looks them up.
    #[tokio::test]
    async fn opening_with_another_key_removes_keys_no_session_uses() {
        let database = TempDatabase::new();
        {
            let store = database.open_with(1);
            login(&store, 1, &alice(), "alice-key").await;
            login(&store, 2, &bob(), "bob-key").await;
            assert!(store.close_session(&token(2)).await.unwrap());
        }
        let keys_of = |user_id: &str| -> i64 {
            database
                .raw()
                .query_row(
                    "SELECT count(*) FROM api_keys WHERE user_id = ?1",
                    params![user_id],
                    |row| row.get(0),
                )
                .unwrap()
        };

        // The same key keeps both.
        drop(database.open_with(1));
        assert_eq!((keys_of("alice-id"), keys_of("bob-id")), (1, 1));

        drop(database.open_with(2));
        assert_eq!((keys_of("alice-id"), keys_of("bob-id")), (1, 0));
    }

    /// A database from before encryption (schema version 1) loses its plain-text keys and
    /// their sessions, and the file no longer holds them: neither the keys still stored nor
    /// those the earlier Bridge deleted, which it left in free pages.
    #[tokio::test]
    async fn plain_text_keys_from_before_encryption_are_dropped() {
        let database = TempDatabase::new();
        {
            let v1 = database.raw();
            v1.execute_batch(MIGRATIONS[0]).unwrap();
            v1.pragma_update(None, "user_version", 1).unwrap();
            for n in 0..200 {
                v1.execute(
                    "INSERT INTO api_keys (library_server, user_id, key_fingerprint, sealed_key)
                     VALUES ('https://abs.lan', ?1, zeroblob(32), CAST(?2 AS BLOB))",
                    params![
                        format!("user-{n}"),
                        format!("legacy-plain-key-{n:04}-{}", "x".repeat(100))
                    ],
                )
                .unwrap();
            }
            v1.execute(
                "INSERT INTO sessions (id, api_key_id) VALUES (?1, 1)",
                params![token(1).as_bytes()],
            )
            .unwrap();
            // As the earlier Bridge revoked keys: without `secure_delete`.
            v1.execute("DELETE FROM api_keys WHERE id > 50", [])
                .unwrap();
            let free_pages: i64 = v1
                .pragma_query_value(None, "freelist_count", |row| row.get(0))
                .unwrap();
            assert!(free_pages > 0, "the deleted keys left no free pages");
        }
        assert!(contains(
            &file_bytes(&database.path),
            b"legacy-plain-key-0150"
        ));

        let store = database.open_with(1);

        assert_eq!(api_key(&store, 1).await, None);
        assert!(!contains(&file_bytes(&database.path), b"legacy-plain-key"));
        login(&store, 2, &alice(), "alice-key").await;
        assert_eq!(api_key(&store, 2).await, Some(ApiKey::new("alice-key")));
    }

    #[test]
    fn refuses_a_database_from_a_newer_bridge() {
        let database = TempDatabase::new();
        let newer = MIGRATIONS.len() as i64 + 1;
        database
            .raw()
            .pragma_update(None, "user_version", newer)
            .unwrap();

        let error = SqliteStore::open(&database.path, &EncryptionKey::new([1; EncryptionKey::LEN]))
            .unwrap_err();
        assert_eq!(error.to_string(), "migrate the schema");
        let cause = std::error::Error::source(&error)
            .expect("a cause")
            .to_string();
        assert!(
            cause.contains(&format!("schema version {newer}")),
            "{cause}"
        );
    }

    #[cfg(unix)]
    mod permissions {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        use super::TempDatabase;

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
            database.open_with(1);
            assert_eq!(mode(&database), 0o600);
        }

        #[test]
        fn an_existing_database_is_made_private() {
            let database = TempDatabase::new();
            std::fs::write(&database.path, b"").unwrap();
            std::fs::set_permissions(&database.path, Permissions::from_mode(0o644)).unwrap();

            database.open_with(1);

            assert_eq!(mode(&database), 0o600);
        }
    }
}
