//! The SQLite implementation of the Bridge's storage traits (ADR 0008).
//!
//! One database file holds all state. Its schema is migrated forward on open; a file written
//! by a newer Bridge (a schema this build does not know) is refused rather than guessed at.
//!
//! API keys are stored encrypted under the operator's [`EncryptionKey`], each bound to its row,
//! and compared and looked up only by a fingerprint keyed by it (see `sealing`). A key that no
//! longer decrypts (the Bridge runs with another encryption key, or the row was tampered with)
//! is removed with its sessions and Handoffs when one of them is looked up: the session is
//! gone, and its user logs in again; the Handoff is gone, and its stream answers no more. On
//! open, the store removes keys sealed under another encryption key that no session or Handoff
//! uses, and warns if any such keys are still used.

mod sealing;

use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use abs::ApiKey;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use store::{
    ActiveHandoff, Error, GroupHandoff, Handoff, HandoffFile, LibraryUser, Session, SessionId,
    Store, StreamId,
};

pub use crate::sealing::EncryptionKey;
use crate::sealing::{Fingerprint, Row, Sealing};

/// Schema migrations in order; the database's `user_version` counts those applied. Append
/// only: a released migration never changes.
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_sessions.sql"),
    include_str!("migrations/0002_encrypted_api_keys.sql"),
    include_str!("migrations/0003_handoffs.sql"),
    include_str!("migrations/0004_handoff_files.sql"),
];

/// A [`Store`] in one SQLite database file. It stays open until it, its [`Closer`]s and its
/// running transactions are all dropped, or a Closer closes it; from then on its transactions
/// fail.
pub struct SqliteStore {
    connection: Arc<SharedConnection>,
    sealing: Arc<Sealing>,
}

/// The connection a store shares with its [`Closer`]s and its transactions on blocking threads.
struct SharedConnection {
    /// `None` once closed.
    connection: Mutex<Option<Connection>>,
    /// Set when closing starts: a transaction that gets the connection after that refuses to
    /// run, also before the closer has taken it.
    closing: AtomicBool,
}

/// A transaction asked of a store that is closed or closing.
#[derive(Debug, thiserror::Error)]
#[error("the database is closed")]
struct ClosedError;

impl fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteStore").finish_non_exhaustive()
    }
}

impl SqliteStore {
    /// Opens the database at `path`, creating it if it does not exist (its directory must),
    /// on Unix makes it readable by its owner only, and migrates its schema. API keys are
    /// stored encrypted under `key`, which is wiped once the store has derived what it needs
    /// from it, before the database is touched.
    pub fn open(path: &Path, key: EncryptionKey) -> Result<Self, Error> {
        let sealing = Sealing::new(key);
        create_private(path)?;
        let mut connection = Connection::open(path).map_err(step("open the database"))?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(step("enable foreign keys"))?;
        // Deleted keys are overwritten rather than left in free pages.
        connection
            .pragma_update(None, "secure_delete", true)
            .map_err(step("enable secure delete"))?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(step("read the schema version"))?;
        if (1..FIRST_ENCRYPTED_SCHEMA).contains(&version) {
            // The earlier Bridge stored keys in plain text and never set `secure_delete`, so
            // keys it deleted may linger in free pages. Rebuilding the file before migrating
            // drops them, and a failure leaves the schema unmigrated, so the next start
            // retries; the migration's own deletes then run under `secure_delete`. The rebuild
            // stays in memory rather than in a temporary file.
            connection
                .execute_batch("PRAGMA temp_store = MEMORY; VACUUM; PRAGMA temp_store = DEFAULT;")
                .map_err(step("drop plain-text keys from free pages"))?;
        }
        migrate(&mut connection).map_err(step("migrate the schema"))?;
        forget_keys_sealed_under_another_key(&connection, &sealing)
            .map_err(step("forget keys sealed under another encryption key"))?;
        Ok(Self {
            connection: Arc::new(SharedConnection {
                connection: Mutex::new(Some(connection)),
                closing: AtomicBool::new(false),
            }),
            sealing: Arc::new(sealing),
        })
    }

    /// A handle that closes this store, also once the store itself has been handed on.
    pub fn closer(&self) -> Closer {
        Closer(Arc::clone(&self.connection))
    }

    /// Runs `work` in one transaction on a blocking thread, committing if it succeeds. Fails if
    /// the store is closed or closing.
    async fn transaction<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, Error> {
        let shared = Arc::clone(&self.connection);
        tokio::task::spawn_blocking(move || -> Result<T, Error> {
            // A panic mid-transaction rolled it back when the transaction dropped, so the
            // connection is still consistent.
            let mut connection = shared
                .connection
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let connection = match connection.as_mut() {
                Some(connection) if !shared.closing.load(Ordering::SeqCst) => connection,
                _ => return Err(Error::new(ClosedError)),
            };
            let run = || -> rusqlite::Result<T> {
                let transaction = connection.transaction()?;
                let result = work(&transaction)?;
                transaction.commit()?;
                Ok(result)
            };
            run().map_err(Error::new)
        })
        .await
        .map_err(Error::new)?
    }

    /// The active Handoff that `query` (a [`select_handoffs`] taking `param` as `?1`) finds,
    /// with its stream; none if its key no longer opens (logged).
    async fn find_handoff(
        &self,
        query: &'static str,
        param: impl rusqlite::ToSql + Send + 'static,
    ) -> Result<Option<GroupHandoff>, Error> {
        let sealing = Arc::clone(&self.sealing);
        let lookup = self
            .transaction(move |tx| {
                let Some((stream, mut handoff, key)) = tx
                    .prepare_cached(query)?
                    .query_row(params![param], handoff_row)
                    .optional()?
                else {
                    return Ok(Lookup::Unknown);
                };
                handoff.files = handoff_files(tx, &stream)?;
                Ok(match key.open(tx, &sealing)? {
                    Some(api_key) => Lookup::Open(GroupHandoff {
                        stream,
                        active: ActiveHandoff { handoff, api_key },
                    }),
                    None => Lookup::Unreadable(key.user),
                })
            })
            .await?;
        Ok(lookup.found())
    }
}

/// Closes a [`SqliteStore`] from outside it, e.g. once the server holding it has stopped.
/// Cloning it is cheap.
#[derive(Clone)]
pub struct Closer(Arc<SharedConnection>);

impl fmt::Debug for Closer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Closer").finish_non_exhaustive()
    }
}

/// How [`Closer::close`] ended.
#[derive(Debug)]
pub enum Closing {
    /// The database is closed, or already was.
    Closed,
    /// Closing the database failed; dropping it retries once; if that fails too, the handle
    /// stays open until the process exits.
    Failed(Error),
    /// A transaction was still running when the bound ran out. The database closes once it
    /// ends, unless the process exits first: whatever it already wrote to the database file is
    /// then rolled back from its journal the next time the database is read.
    TransactionStillRunning,
}

impl Closer {
    /// Closes the store: transactions not yet running fail from now on, and a transaction
    /// already running gets up to `bound` to finish; `close` returns then, closed or not (see
    /// [`Closing`]).
    /// Transactions run on blocking threads of whichever runtime asked for them, so this covers
    /// them all, whichever runtime that is.
    pub fn close(&self, bound: Duration) -> Closing {
        self.0.closing.store(true, Ordering::SeqCst);
        let shared = Arc::clone(&self.0);
        let (done, closed) = mpsc::channel();
        // A std `Mutex` has no timed lock: wait for the connection on a thread of its own.
        std::thread::spawn(move || {
            let connection = shared
                .connection
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            let closed =
                connection.map(|connection| connection.close().map_err(|(_, error)| error));
            // The closer stops listening once its bound has run out.
            let _ = done.send(closed);
        });
        match closed.recv_timeout(bound) {
            Ok(None | Some(Ok(()))) => Closing::Closed,
            Ok(Some(Err(error))) => Closing::Failed(Error::new(error)),
            Err(mpsc::RecvTimeoutError::Timeout) => Closing::TransactionStillRunning,
            Err(mpsc::RecvTimeoutError::Disconnected) => Closing::Failed(Error::new(ClosePanicked)),
        }
    }
}

/// The thread closing the database panicked.
#[derive(Debug, thiserror::Error)]
#[error("closing the database panicked")]
struct ClosePanicked;

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

/// The schema version (`user_version`) from which API keys are stored encrypted.
const FIRST_ENCRYPTED_SCHEMA: i64 = 2;

fn migrate(connection: &mut Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
        return Ok(());
    }
    for migration in &MIGRATIONS[applied..] {
        transaction.execute_batch(migration)?;
    }
    transaction.pragma_update(None, "user_version", known as i64)?;
    transaction.commit()?;
    Ok(())
}

/// Removes keys sealed under another encryption key than `sealing`'s that no session or
/// Handoff uses (nothing would ever look them up again), and warns once each if others remain:
/// keys with sessions (those end at their next call, and their users log in again), and keys
/// Handoffs use (those Handoffs end when their streams are next asked for, or listed). A key
/// both use is counted in both.
fn forget_keys_sealed_under_another_key(
    connection: &Connection,
    sealing: &Sealing,
) -> rusqlite::Result<()> {
    let removed = connection.execute(
        "DELETE FROM api_keys
         WHERE key_id <> ?1
           AND NOT EXISTS (SELECT 1 FROM sessions WHERE sessions.api_key_id = api_keys.id)
           AND NOT EXISTS (SELECT 1 FROM handoffs WHERE handoffs.api_key_id = api_keys.id)",
        params![sealing.key_id()],
    )?;
    if removed > 0 {
        tracing::info!(
            removed,
            "removed stored API keys that were encrypted with another encryption key and that \
             no session or Handoff used"
        );
    }
    let (with_sessions, with_handoffs): (i64, i64) = connection.query_row(
        "SELECT
             count(*) FILTER (WHERE EXISTS
                 (SELECT 1 FROM sessions WHERE sessions.api_key_id = api_keys.id)),
             count(*) FILTER (WHERE EXISTS
                 (SELECT 1 FROM handoffs WHERE handoffs.api_key_id = api_keys.id))
         FROM api_keys WHERE key_id <> ?1",
        params![sealing.key_id()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if with_sessions > 0 {
        tracing::warn!(
            keys = with_sessions,
            "stored API keys were encrypted with another encryption key; their sessions end \
             at their next call, and their users must log in again"
        );
    }
    if with_handoffs > 0 {
        tracing::warn!(
            keys = with_handoffs,
            "stored API keys Handoffs use were encrypted with another encryption key; those \
             Handoffs end"
        );
    }
    Ok(())
}

/// Stores `api_key` (`sealed`, with its `fingerprint`, under the encryption key `key_id`) for
/// `user` unless it already is, and returns its row's id. A key already stored for the user
/// keeps its row and is sealed anew.
fn store_key(
    tx: &Transaction<'_>,
    user: &LibraryUser,
    fingerprint: &Fingerprint,
    sealed: &[u8],
    key_id: &[u8; 16],
) -> rusqlite::Result<i64> {
    tx.prepare_cached(
        "INSERT INTO api_keys (library_server, user_id, key_fingerprint, sealed_key, key_id)
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
    )
}

/// A stored key as read with what uses it: its row's id, user and fingerprint, and the sealed
/// key.
struct StoredKey {
    id: i64,
    user: LibraryUser,
    fingerprint: Fingerprint,
    sealed: Vec<u8>,
}

impl StoredKey {
    /// Reads the key from `row`, from column `first` on: `api_keys`' `id`, `library_server`,
    /// `user_id`, `key_fingerprint` and `sealed_key`.
    fn read(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(first)?,
            user: LibraryUser {
                library_server: row.get(first + 1)?,
                user_id: row.get(first + 2)?,
            },
            fingerprint: row.get(first + 3)?,
            sealed: row.get(first + 4)?,
        })
    }

    /// The key, if it opens with `sealing`; otherwise it is removed in `tx`, with its sessions
    /// and Handoffs, and none.
    fn open(&self, tx: &Transaction<'_>, sealing: &Sealing) -> rusqlite::Result<Option<ApiKey>> {
        if let Some(api_key) = sealing.open(&Row::of(&self.user, &self.fingerprint), &self.sealed) {
            return Ok(Some(api_key));
        }
        tx.prepare_cached("DELETE FROM api_keys WHERE id = ?1")?
            .execute(params![self.id])?;
        Ok(None)
    }
}

/// Logs that the key of `user`, which did not open, was removed with what used it.
fn warn_unreadable(user: &LibraryUser) {
    tracing::warn!(
        library_server = user.library_server,
        user_id = user.user_id,
        "a stored API key does not decrypt with the current encryption key (or was tampered \
         with); it was removed, and the sessions and Handoffs using it ended"
    );
}

/// A query of Handoffs with their keys, ending in `$rest`: from `handoffs h` joined with
/// `api_keys k`, the columns [`handoff_row`] reads.
macro_rules! select_handoffs {
    ($rest:literal) => {
        concat!(
            "SELECT h.group_id, h.library_item, h.episode, h.username, h.started_at, k.id, \
             k.library_server, k.user_id, k.key_fingerprint, k.sealed_key, h.stream_id \
             FROM handoffs h JOIN api_keys k ON k.id = h.api_key_id ",
            $rest
        )
    };
}

/// A Handoff's stream, the Handoff without its files ([`handoff_files`] reads them) and its
/// stored key, from a row of [`select_handoffs`].
fn handoff_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(StreamId, Handoff, StoredKey)> {
    let key = StoredKey::read(row, 5)?;
    let stream: [u8; 32] = row.get(10)?;
    let handoff = Handoff {
        group: row.get(0)?,
        library_item: row.get(1)?,
        episode: row.get(2)?,
        files: Vec::new(),
        started_by: key.user.clone(),
        username: row.get(3)?,
        started_at: row.get(4)?,
    };
    Ok((StreamId::from_bytes(stream), handoff, key))
}

/// The files of the Handoff whose stream is `stream`, in playing order.
fn handoff_files(tx: &Transaction<'_>, stream: &StreamId) -> rusqlite::Result<Vec<HandoffFile>> {
    tx.prepare_cached(
        "SELECT ino, name, duration_ns FROM handoff_files WHERE stream_id = ?1
         ORDER BY file_index",
    )?
    .query_map(params![stream.as_bytes()], |row| {
        let duration: Option<i64> = row.get(2)?;
        Ok(HandoffFile {
            ino: row.get(0)?,
            name: row.get(1)?,
            // The column holds no negative length.
            duration: duration.map(|nanos| Duration::from_nanos(nanos.unsigned_abs())),
        })
    })?
    .collect()
}

/// A session or Handoff looked up in the database.
enum Lookup<T> {
    Open(T),
    /// Its key does not open: it was removed, with what used it.
    Unreadable(LibraryUser),
    Unknown,
}

impl<T> Lookup<T> {
    /// What was found, if its key opened; an unreadable key is logged.
    fn found(self) -> Option<T> {
        match self {
            Self::Open(found) => Some(found),
            Self::Unreadable(user) => {
                warn_unreadable(&user);
                None
            }
            Self::Unknown => None,
        }
    }
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
            let api_key_id = store_key(tx, &user, &fingerprint, &sealed, &key_id)?;
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
                let Some(key) = tx
                    .prepare_cached(
                        "SELECT k.id, k.library_server, k.user_id, k.key_fingerprint, k.sealed_key
                         FROM sessions s JOIN api_keys k ON k.id = s.api_key_id
                         WHERE s.id = ?1",
                    )?
                    .query_row(params![id.as_bytes()], |row| StoredKey::read(row, 0))
                    .optional()?
                else {
                    return Ok(Lookup::Unknown);
                };
                Ok(match key.open(tx, &sealing)? {
                    Some(api_key) => Lookup::Open(Session {
                        user: key.user,
                        api_key,
                    }),
                    None => Lookup::Unreadable(key.user),
                })
            })
            .await?;
        Ok(lookup.found())
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

    async fn start_handoff(
        &self,
        stream: &StreamId,
        handoff: &Handoff,
        api_key: &ApiKey,
    ) -> Result<(), Error> {
        let (stream, handoff) = (*stream, handoff.clone());
        let fingerprint = self.sealing.fingerprint(api_key);
        let sealed = self
            .sealing
            .seal(&Row::of(&handoff.started_by, &fingerprint), api_key)
            .map_err(Error::new)?;
        let key_id = *self.sealing.key_id();
        self.transaction(move |tx| {
            let api_key_id = store_key(tx, &handoff.started_by, &fingerprint, &sealed, &key_id)?;
            tx.prepare_cached("DELETE FROM handoffs WHERE group_id = ?1")?
                .execute(params![handoff.group])?;
            tx.prepare_cached(
                "INSERT INTO handoffs (group_id, stream_id, library_item, episode, username,
                     api_key_id, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                handoff.group,
                stream.as_bytes(),
                handoff.library_item,
                handoff.episode,
                handoff.username,
                api_key_id,
                handoff.started_at,
            ])?;
            let mut insert = tx.prepare_cached(
                "INSERT INTO handoff_files (stream_id, file_index, ino, name, duration_ns)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (index, file) in (0_i64..).zip(&handoff.files) {
                let duration = file
                    .duration
                    .map(|duration| i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX));
                insert.execute(params![
                    stream.as_bytes(),
                    index,
                    file.ino,
                    file.name,
                    duration
                ])?;
            }
            Ok(())
        })
        .await
    }

    async fn handoff(&self, stream: &StreamId) -> Result<Option<ActiveHandoff>, Error> {
        let query = select_handoffs!("WHERE h.stream_id = ?1");
        let found = self.find_handoff(query, *stream.as_bytes()).await?;
        Ok(found.map(|found| found.active))
    }

    async fn group_handoff(&self, group: &str) -> Result<Option<GroupHandoff>, Error> {
        let query = select_handoffs!("WHERE h.group_id = ?1");
        self.find_handoff(query, group.to_owned()).await
    }

    async fn handoffs(&self) -> Result<Vec<Handoff>, Error> {
        let sealing = Arc::clone(&self.sealing);
        let (handoffs, unreadable) = self
            .transaction(move |tx| {
                let rows = tx
                    .prepare_cached(select_handoffs!("ORDER BY h.started_at, h.group_id"))?
                    .query_map([], handoff_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut handoffs = Vec::new();
                let mut unreadable: Vec<StoredKey> = Vec::new();
                for (stream, mut handoff, key) in rows {
                    // Handoffs may share a key; once it was removed, the others went with it.
                    if unreadable.iter().any(|removed| removed.id == key.id) {
                        continue;
                    }
                    match key.open(tx, &sealing)? {
                        Some(_) => {
                            handoff.files = handoff_files(tx, &stream)?;
                            handoffs.push(handoff);
                        }
                        None => unreadable.push(key),
                    }
                }
                Ok((handoffs, unreadable))
            })
            .await?;
        for key in &unreadable {
            warn_unreadable(&key.user);
        }
        Ok(handoffs)
    }

    async fn end_handoff(&self, stream: &StreamId) -> Result<bool, Error> {
        let stream = *stream;
        self.transaction(move |tx| {
            let deleted = tx
                .prepare_cached("DELETE FROM handoffs WHERE stream_id = ?1")?
                .execute(params![stream.as_bytes()])?;
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
    use store::{Handoff, HandoffFile, LibraryUser, SessionId, Store, StreamId};
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
            SqliteStore::open(&self.path, EncryptionKey::new(&[n; EncryptionKey::LEN]))
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

    fn kitchen_handoff(user: &LibraryUser) -> Handoff {
        Handoff {
            group: "RINCON_K".into(),
            library_item: "podcast-id".into(),
            episode: Some("episode-id".into()),
            files: vec![HandoffFile {
                ino: "42".into(),
                name: "Episode 1.mp3".into(),
                duration: Some(std::time::Duration::from_secs(600)),
            }],
            started_by: user.clone(),
            username: "alice".into(),
            started_at: 1_000,
        }
    }

    async fn start_kitchen(store: &SqliteStore, stream: &str, user: &LibraryUser, key: &str) {
        store
            .start_handoff(
                &StreamId::of(stream),
                &kitchen_handoff(user),
                &ApiKey::new(key),
            )
            .await
            .unwrap();
    }

    /// A Handoff whose key no longer decrypts is gone, listed or looked up, and stays gone:
    /// its key was removed, not just hidden.
    #[tokio::test]
    async fn another_encryption_key_ends_the_handoffs() {
        let database = TempDatabase::new();
        start_kitchen(&database.open_with(1), "stream-1", &alice(), "alice-key").await;
        {
            // A key only a Handoff uses stays on open, until the Handoff is looked at.
            let other = database.open_with(2);
            assert_eq!(other.handoffs().await.unwrap(), []);
        }
        let store = database.open_with(1);
        assert_eq!(
            store.handoff(&StreamId::of("stream-1")).await.unwrap(),
            None
        );
        assert_eq!(store.handoffs().await.unwrap(), []);

        start_kitchen(&database.open_with(1), "stream-2", &alice(), "alice-key").await;
        let other = database.open_with(2);
        assert_eq!(
            other.handoff(&StreamId::of("stream-2")).await.unwrap(),
            None
        );
        assert_eq!(database.open_with(1).handoffs().await.unwrap(), []);
    }

    /// Opening with another encryption key keeps a key only a Handoff uses (it is not unused),
    /// so going back to the right encryption key finds the Handoff still active.
    #[tokio::test]
    async fn a_key_only_a_handoff_uses_survives_opening_with_another_encryption_key() {
        let database = TempDatabase::new();
        start_kitchen(&database.open_with(1), "stream-1", &alice(), "alice-key").await;

        drop(database.open_with(2));

        let store = database.open_with(1);
        let active = store.handoff(&StreamId::of("stream-1")).await.unwrap();
        assert_eq!(
            active.map(|active| active.api_key),
            Some(ApiKey::new("alice-key"))
        );
    }

    #[tokio::test]
    async fn a_tampered_key_ends_the_handoffs_using_it_only() {
        let database = TempDatabase::new();
        let store = database.open_with(1);
        start_kitchen(&store, "stream-1", &alice(), "alice-key").await;
        let office = Handoff {
            group: "RINCON_O".into(),
            ..kitchen_handoff(&bob())
        };
        store
            .start_handoff(&StreamId::of("stream-2"), &office, &ApiKey::new("bob-key"))
            .await
            .unwrap();
        let mut sealed = database.sealed_key("alice-id");
        sealed[30] ^= 1;
        database.set_sealed_key("alice-id", &sealed);

        assert_eq!(store.handoffs().await.unwrap(), [office]);
        assert_eq!(
            store.handoff(&StreamId::of("stream-1")).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_groups_handoff_with_a_tampered_key_is_none_and_the_key_is_forgotten() {
        let database = TempDatabase::new();
        let store = database.open_with(1);
        login(&store, 1, &alice(), "alice-key").await;
        start_kitchen(&store, "stream-1", &alice(), "alice-key").await;
        let mut sealed = database.sealed_key("alice-id");
        sealed[30] ^= 1;
        database.set_sealed_key("alice-id", &sealed);

        assert_eq!(store.group_handoff("RINCON_K").await.unwrap(), None);
        // Forgotten by that lookup, with the session and the Handoff using it.
        let keys: i64 = database
            .raw()
            .query_row("SELECT count(*) FROM api_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(keys, 0);
        assert_eq!(api_key(&store, 1).await, None);
        assert_eq!(store.handoffs().await.unwrap(), []);
    }

    #[tokio::test]
    async fn the_file_holds_no_stream_id_and_no_plain_key() {
        let database = TempDatabase::new();
        let store = database.open_with(1);
        start_kitchen(&store, "stream-secret-id", &alice(), "alice-secret-key").await;
        drop(store);

        let bytes = file_bytes(&database.path);
        assert!(!contains(&bytes, b"stream-secret-id"));
        assert!(!contains(&bytes, b"alice-secret-key"));
    }

    /// A database from before Handoffs (schema version 2) keeps its sessions.
    #[tokio::test]
    async fn sessions_from_before_handoffs_stay() {
        let database = TempDatabase::new();
        login(&database.open_with(1), 1, &alice(), "alice-key").await;
        // Back to how a Bridge of schema version 2 left it.
        database
            .raw()
            .execute_batch(
                "DROP TABLE handoff_files; DROP TABLE handoffs; PRAGMA user_version = 2;",
            )
            .unwrap();

        let store = database.open_with(1);
        assert_eq!(schema_version(&database), MIGRATIONS.len() as i64);
        assert_eq!(api_key(&store, 1).await, Some(ApiKey::new("alice-key")));
        assert_eq!(store.handoffs().await.unwrap(), []);
    }

    /// A Handoff stored by a Bridge of schema version 3, with one file, keeps it, of unknown
    /// length.
    #[tokio::test]
    async fn a_handoff_from_before_its_files_were_listed_keeps_its_one_file() {
        let database = TempDatabase::new();
        start_kitchen(&database.open_with(1), "stream-1", &alice(), "alice-key").await;
        // Back to how a Bridge of schema version 3 left it: the file in the Handoff's row.
        database
            .raw()
            .execute_batch(
                "ALTER TABLE handoffs ADD COLUMN file_ino TEXT;
                 ALTER TABLE handoffs ADD COLUMN file_name TEXT;
                 UPDATE handoffs SET file_ino = '42', file_name = 'Episode 1.mp3';
                 DROP TABLE handoff_files;
                 PRAGMA user_version = 3;",
            )
            .unwrap();

        let store = database.open_with(1);
        assert_eq!(schema_version(&database), MIGRATIONS.len() as i64);
        let active = store.handoff(&StreamId::of("stream-1")).await.unwrap();
        let files = active.map(|active| active.handoff.files);
        let file = HandoffFile {
            ino: "42".into(),
            name: "Episode 1.mp3".into(),
            duration: None,
        };
        assert_eq!(files, Some(vec![file]));
    }

    /// Writes a database as the Bridge before encryption (schema version 1) left it: plain-text
    /// keys, one session, and most keys deleted without `secure_delete`, so their plain text
    /// lingers in free pages.
    fn seed_plain_text_database(database: &TempDatabase) {
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
        v1.execute("DELETE FROM api_keys WHERE id > 50", [])
            .unwrap();
        let free_pages: i64 = v1
            .pragma_query_value(None, "freelist_count", |row| row.get(0))
            .unwrap();
        assert!(free_pages > 0, "the deleted keys left no free pages");
        drop(v1);
        assert!(contains(
            &file_bytes(&database.path),
            b"legacy-plain-key-0150"
        ));
    }

    fn schema_version(database: &TempDatabase) -> i64 {
        database
            .raw()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap()
    }

    /// A database from before encryption (schema version 1) loses its plain-text keys and
    /// their sessions, and the file no longer holds them: neither the keys still stored nor
    /// those the earlier Bridge deleted, which it left in free pages.
    #[tokio::test]
    async fn plain_text_keys_from_before_encryption_are_dropped() {
        let database = TempDatabase::new();
        seed_plain_text_database(&database);

        let store = database.open_with(1);

        assert_eq!(api_key(&store, 1).await, None);
        assert!(!contains(&file_bytes(&database.path), b"legacy-plain-key"));
        login(&store, 2, &alice(), "alice-key").await;
        assert_eq!(api_key(&store, 2).await, Some(ApiKey::new("alice-key")));
    }

    /// If rebuilding the file fails (here: another connection is reading it), nothing is
    /// migrated, so the next open rebuilds it and the plain text still goes.
    #[test]
    fn a_failed_rebuild_is_retried_at_the_next_open() {
        let database = TempDatabase::new();
        seed_plain_text_database(&database);
        let reader = database.raw();
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM api_keys;")
            .unwrap();

        let error = SqliteStore::open(&database.path, EncryptionKey::new(&[1; EncryptionKey::LEN]))
            .unwrap_err();
        assert_eq!(error.to_string(), "drop plain-text keys from free pages");
        assert_eq!(schema_version(&database), 1);

        drop(reader);
        drop(database.open_with(1));
        assert_eq!(schema_version(&database), MIGRATIONS.len() as i64);
        assert!(!contains(&file_bytes(&database.path), b"legacy-plain-key"));
    }

    #[test]
    fn refuses_a_database_from_a_newer_bridge() {
        let database = TempDatabase::new();
        let newer = MIGRATIONS.len() as i64 + 1;
        database
            .raw()
            .pragma_update(None, "user_version", newer)
            .unwrap();

        let error = SqliteStore::open(&database.path, EncryptionKey::new(&[1; EncryptionKey::LEN]))
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

    mod close {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        use super::TempDatabase;
        use crate::Closing;

        fn has_probe_table(database: &TempDatabase) -> bool {
            database
                .raw()
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name = 'probe'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
                == 1
        }

        fn journal_exists(database: &TempDatabase) -> bool {
            database.path.with_file_name("bridge.db-journal").exists()
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn close_lets_a_running_transaction_commit_first() {
            let database = TempDatabase::new();
            let store = database.open_with(1);
            let closer = store.closer();
            let (started, running) = mpsc::channel();
            let transaction = tokio::spawn(async move {
                store
                    .transaction(move |tx| {
                        started.send(()).unwrap();
                        std::thread::sleep(Duration::from_millis(300));
                        tx.execute("CREATE TABLE probe (x)", [])?;
                        Ok(())
                    })
                    .await
            });
            running.recv().unwrap();

            let closed = closer.close(Duration::from_secs(5));

            assert!(matches!(closed, Closing::Closed), "{closed:?}");
            transaction.await.unwrap().expect("the transaction commits");
            assert!(has_probe_table(&database));
            assert!(!journal_exists(&database));
        }

        #[tokio::test]
        async fn a_transaction_after_close_fails() {
            let database = TempDatabase::new();
            let store = database.open_with(1);

            let closed = store.closer().close(Duration::from_secs(1));

            assert!(matches!(closed, Closing::Closed), "{closed:?}");
            let error = store
                .transaction(|tx| tx.execute("CREATE TABLE probe (x)", []))
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), "the database is closed");
            assert!(!has_probe_table(&database));
            // Closing again finds nothing left to close.
            assert!(matches!(
                store.closer().close(Duration::from_secs(1)),
                Closing::Closed
            ));
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn close_gives_up_at_its_bound_on_a_transaction_still_running() {
            let database = TempDatabase::new();
            let store = database.open_with(1);
            let closer = store.closer();
            // Another connection's lock keeps the store's write waiting (for SQLite's busy
            // timeout, 5 s) far longer than the bound.
            let other = database.raw();
            other.execute_batch("BEGIN EXCLUSIVE;").unwrap();
            let (started, running) = mpsc::channel();
            let transaction = tokio::spawn(async move {
                store
                    .transaction(move |tx| {
                        started.send(()).unwrap();
                        tx.execute("CREATE TABLE probe (x)", [])
                    })
                    .await
            });
            running.recv().unwrap();

            let bound = Duration::from_millis(300);
            let asked = Instant::now();
            let closed = closer.close(bound);
            let took = asked.elapsed();

            assert!(
                matches!(closed, Closing::TransactionStillRunning),
                "{closed:?}"
            );
            assert!(took >= bound, "gave up after {took:?}");
            assert!(took < bound + Duration::from_secs(1), "took {took:?}");
            other.execute_batch("ROLLBACK;").unwrap();
            transaction
                .await
                .unwrap()
                .expect("the transaction commits once the lock is gone");
        }

        /// A transaction queued for the connection behind a running one when the close starts
        /// fails, even if it gets the connection before the closer does (a std `Mutex` isn't
        /// fair): the running one commits, the queued one doesn't run.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_transaction_queued_behind_a_running_one_at_close_fails() {
            let database = TempDatabase::new();
            let store = std::sync::Arc::new(database.open_with(1));
            let closer = store.closer();
            let (started, running) = mpsc::channel();
            let (finish, finishing) = mpsc::channel::<()>();
            let first = tokio::spawn({
                let store = std::sync::Arc::clone(&store);
                async move {
                    store
                        .transaction(move |tx| {
                            started.send(()).unwrap();
                            finishing.recv().unwrap();
                            tx.execute("CREATE TABLE probe (x)", [])?;
                            Ok(())
                        })
                        .await
                }
            });
            running.recv().unwrap();
            let second = tokio::spawn({
                let store = std::sync::Arc::clone(&store);
                async move {
                    store
                        .transaction(|tx| tx.execute("CREATE TABLE queued (x)", []))
                        .await
                }
            });
            // Nothing marks a thread as waiting for a lock. This pause doesn't decide the
            // outcome; it lets the second transaction queue on the connection before the
            // closer does, so that it, not the closer, most likely gets the connection next.
            std::thread::sleep(Duration::from_millis(100));

            let closing = std::thread::spawn({
                let closer = closer.clone();
                move || closer.close(Duration::from_secs(5))
            });
            while !closer.0.closing.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::yield_now();
            }
            finish.send(()).unwrap();
            let closed = closing.join().unwrap();

            assert!(matches!(closed, Closing::Closed), "{closed:?}");
            first
                .await
                .unwrap()
                .expect("the running transaction commits");
            let error = second.await.unwrap().unwrap_err();
            assert_eq!(error.to_string(), "the database is closed");
            assert!(has_probe_table(&database));
        }

        /// Once a close has started, a transaction that gets the connection before the closer
        /// does fails: the closing flag decides, not whether the connection is gone yet.
        #[tokio::test]
        async fn a_transaction_that_gets_the_connection_while_closing_fails() {
            let database = TempDatabase::new();
            let store = database.open_with(1);
            // A close has begun but hasn't taken the connection yet.
            store
                .closer()
                .0
                .closing
                .store(true, std::sync::atomic::Ordering::SeqCst);

            let error = store
                .transaction(|tx| tx.execute("CREATE TABLE probe (x)", []))
                .await
                .unwrap_err();

            assert_eq!(error.to_string(), "the database is closed");
            assert!(!has_probe_table(&database));
        }
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
