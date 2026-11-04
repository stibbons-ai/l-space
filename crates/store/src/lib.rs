//! The Bridge's storage traits (ADR 0008).
//!
//! Code that uses the Bridge's persistent state depends only on [`Store`], never on a backend,
//! so replacing the backend replaces only the implementation. Every implementation must pass
//! the shared contract suite in `contract` (feature `contract`).
//!
//! What is stored so far: each Audiobookshelf user's API key per Library Server, and App
//! sessions. A session is bound to the API key it was opened with: it ends when that key is
//! replaced or revoked.

#[cfg(feature = "contract")]
pub mod contract;

use abs::ApiKey;
use sha2::{Digest, Sha256};

/// An Audiobookshelf user on one Library Server: user ids are only unique per server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryUser {
    /// The Library Server's normalized base URL (ADR 0006).
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
    /// Opens session `id` for `user`, acting with `api_key`.
    ///
    /// `api_key` becomes the user's stored key. If a different key was stored for the user, it
    /// is replaced and every session opened with it ends.
    fn open_session(
        &self,
        id: &SessionId,
        user: &LibraryUser,
        api_key: &ApiKey,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// The open session `id`, if any.
    fn session(
        &self,
        id: &SessionId,
    ) -> impl Future<Output = Result<Option<Session>, Error>> + Send;

    /// Ends session `id`. Returns whether it was open.
    fn close_session(&self, id: &SessionId) -> impl Future<Output = Result<bool, Error>> + Send;

    /// Forgets `user`'s stored key, ending every session opened with it, but only if it is still
    /// `api_key`: a key the Library Server rejected may meanwhile have been replaced by a new
    /// login, whose sessions must stay.
    fn revoke_api_key(
        &self,
        user: &LibraryUser,
        api_key: &ApiKey,
    ) -> impl Future<Output = Result<(), Error>> + Send;
}
