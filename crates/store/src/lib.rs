//! The Bridge's storage traits (ADR 0008).
//!
//! Code that uses the Bridge's persistent state depends only on [`Store`], never on a backend,
//! so replacing the backend replaces only the implementation. Every implementation must pass
//! the shared contract suite in `contract` (feature `contract`).
//!
//! What is stored so far: the Audiobookshelf API keys users logged in with, per Library Server
//! and user, App sessions, and active Handoffs. A session is bound to the API key it was
//! opened with and ends when that key is revoked; a user's other keys and their sessions are
//! unaffected. Logout ends only the session: the key stays stored (later work acts for the
//! user with it), as do keys and sessions of Library Servers the operator has since removed.
//!
//! A Handoff acts with the API key of the user who started it, stored like a session's, and
//! ends when that key is revoked or can no longer be read; logging out doesn't end it. A Group
//! has at most one: starting another on it replaces the one it had.

#[cfg(feature = "contract")]
pub mod contract;

use abs::ApiKey;
use sha2::{Digest, Sha256};

/// An Audiobookshelf user on one Library Server: user ids are only unique per server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryUser {
    /// The Library Server's normalized base URL (ADR 0006). Callers pass the `base_url` of a
    /// configured Library Server, so every spelling of one server is one user.
    pub library_server: String,
    /// Audiobookshelf's id for the user.
    pub user_id: String,
}

/// Identifies an App session: the SHA-256 of its token. The token itself is never stored, so
/// whoever reads the store cannot use the sessions in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionId([u8; 32]);

impl SessionId {
    /// The id of the session whose token is `token`.
    pub fn of_token(token: &str) -> Self {
        Self(Sha256::digest(token.as_bytes()).into())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// An open session: whose it is and the API key it acts with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub user: LibraryUser,
    pub api_key: ApiKey,
}

/// Identifies a Handoff's stream: the SHA-256 of the random id in its URL. The id itself is
/// never stored, so whoever reads the store cannot use the streams in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamId([u8; 32]);

impl StreamId {
    /// The stream whose URL carries `id`.
    pub fn of(id: &str) -> Self {
        Self(Sha256::digest(id.as_bytes()).into())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// A Handoff (ADR 0008): an Item started on a Group by a user, playing from the Bridge's
/// stream (ADR 0010).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    /// The Group's id: its coordinator's UUID.
    pub group: String,
    /// The audiobook, or the podcast of the episode, on the starter's Library Server.
    pub library_item: String,
    /// The episode, for a podcast's.
    pub episode: Option<String>,
    /// The audio file the stream serves.
    pub file: HandoffFile,
    /// Who started it, and acts for it.
    pub started_by: LibraryUser,
    /// The starter's username when they started it.
    pub username: String,
    /// When it started, in milliseconds since the Unix epoch.
    pub started_at: i64,
}

/// The audio file a Handoff's stream serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffFile {
    /// Audiobookshelf's id for the file within its library item.
    pub ino: String,
    /// Its name, with its extension: the last segment of the stream's URL.
    pub name: String,
}

/// A Handoff as its stream needs it: with the API key it acts with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveHandoff {
    pub handoff: Handoff,
    pub api_key: ApiKey,
}

/// A storage failure. The store's state is unchanged by the failed operation.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct Error(Box<dyn std::error::Error + Send + Sync>);

impl Error {
    /// Wraps `source`. The wrapped error and its sources are logged; they must not carry
    /// secrets or row data.
    pub fn new(source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(source.into())
    }
}

/// The Bridge's persistent state. Every operation is atomic, and what it wrote survives
/// reopening the store.
pub trait Store: Send + Sync + 'static {
    /// Opens session `id` for `user`, acting with `api_key`, which is stored for `user` unless
    /// it already is. Other keys stored for `user`, and their sessions, stay.
    fn open_session(
        &self,
        id: &SessionId,
        user: &LibraryUser,
        api_key: &ApiKey,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// The open session `id`, if any. A session whose stored key can no longer be read (an
    /// implementation that encrypts keys runs with another encryption key, or the stored key
    /// was tampered with) is not open: the implementation ends it, and its user logs in again.
    ///
    /// Whoever acts with the session's key must check that Audiobookshelf still reports the
    /// session's user for it: an admin can move a key to another user.
    fn session(
        &self,
        id: &SessionId,
    ) -> impl Future<Output = Result<Option<Session>, Error>> + Send;

    /// Ends session `id`. Returns whether it was open.
    fn close_session(&self, id: &SessionId) -> impl Future<Output = Result<bool, Error>> + Send;

    /// Forgets `api_key` for `user` on `user`'s Library Server, ending every session opened
    /// with it and every Handoff acting with it. The user's other keys, and the same key
    /// stored for another Library Server or user, stay. Returns whether it was still stored;
    /// false if there was nothing to revoke (a concurrent revoke came first, for example).
    fn revoke_api_key(
        &self,
        user: &LibraryUser,
        api_key: &ApiKey,
    ) -> impl Future<Output = Result<bool, Error>> + Send;

    /// Starts `handoff`, its stream identified by `stream`, acting with `api_key`, which is
    /// stored for the starter unless it already is. It replaces the Handoff on the same Group,
    /// if any, whose stream is then gone.
    fn start_handoff(
        &self,
        stream: &StreamId,
        handoff: &Handoff,
        api_key: &ApiKey,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// The active Handoff whose stream is `stream`, if any, with the key it acts with. A
    /// Handoff whose key can no longer be read is not active: as for a session, the
    /// implementation forgets the key, ending its sessions and Handoffs.
    fn handoff(
        &self,
        stream: &StreamId,
    ) -> impl Future<Output = Result<Option<ActiveHandoff>, Error>> + Send;

    /// Every active Handoff, the earliest started first (ties by Group id). As for
    /// [`Store::handoff`], a Handoff whose key can no longer be read is not listed, and the
    /// implementation forgets that key, ending its sessions and Handoffs.
    fn handoffs(&self) -> impl Future<Output = Result<Vec<Handoff>, Error>> + Send;

    /// Ends the Handoff whose stream is `stream`. Returns whether it was active.
    fn end_handoff(&self, stream: &StreamId) -> impl Future<Output = Result<bool, Error>> + Send;
}
