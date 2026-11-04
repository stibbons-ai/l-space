//! The Bridge's storage traits (ADR 0008).
//!
//! Code that uses the Bridge's persistent state depends only on [`Store`], never on a backend,
//! so replacing the backend replaces only the implementation. Every implementation must pass
//! the shared contract suite in `contract` (feature `contract`).
//!
//! What is stored so far: the Audiobookshelf API keys users logged in with, per Library Server
//! and user, and App sessions. A session is bound to the API key it was opened with and ends
//! when that key is revoked; a user's other keys and their sessions are unaffected. Logout
//! ends only the session: the key stays stored (later work acts for the user with it), as do
//! keys and sessions of Library Servers the operator has since removed.

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
/// whoever reads the store cannot use the sessions in it (it still holds the API keys in plain
/// text).
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

/// A storage failure. The store's state is unchanged by the failed operation.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct Error(Box<dyn std::error::Error + Send + Sync>);

impl Error {
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

    /// The open session `id`, if any.
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
    /// with it. The user's other keys, and the same key stored for another Library Server or
    /// user, stay.
    fn revoke_api_key(
        &self,
        user: &LibraryUser,
        api_key: &ApiKey,
    ) -> impl Future<Output = Result<(), Error>> + Send;
}
