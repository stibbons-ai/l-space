//! The App API's view of this Bridge: configured Library Servers, logins and sessions.

use std::fmt::Write;

use api::server::{BoxFuture, Bridge};
use api::{ApiError, CurrentUser, LibraryServer, Session, SessionToken};
use store::{LibraryUser, SessionId, Store};

use crate::config::{LibraryServerConfig, LibraryServerUrl};

/// Serves the App API from the configured Library Servers and the [`Store`].
#[derive(Debug)]
pub struct AppApi<S> {
    library_servers: Vec<LibraryServer>,
    abs: abs::Client,
    store: S,
}

impl<S: Store> AppApi<S> {
    pub fn new(library_servers: Vec<LibraryServerConfig>, store: S) -> Self {
        let library_servers = library_servers
            .into_iter()
            .map(|server| LibraryServer {
                base_url: server.url.into_string(),
                name: server.name,
            })
            .collect();
        Self {
            library_servers,
            abs: abs::Client::new(),
            store,
        }
    }

    /// The base URL of the configured Library Server that `url` (in any spelling) normalizes
    /// to.
    fn configured(&self, url: &str) -> Option<&str> {
        self.configured_base_url(&LibraryServerUrl::parse(url).ok()?.into_string())
    }

    /// `base_url` (already normalized), if it is a configured Library Server's.
    fn configured_base_url(&self, base_url: &str) -> Option<&str> {
        self.library_servers
            .iter()
            .map(|server| server.base_url.as_str())
            .find(|&configured| configured == base_url)
    }

    async fn login(
        &self,
        library_server: String,
        api_key: api::ApiKey,
    ) -> Result<Session, ApiError> {
        let library_server = self
            .configured(&library_server)
            .ok_or(ApiError::LibraryServerNotConfigured)?;
        let api_key = abs::ApiKey::from(api_key);
        let user = self
            .abs
            .me(library_server, &api_key)
            .await
            .map_err(|error| abs_error(library_server, error))?;

        let token = new_session_token()?;
        let owner = LibraryUser {
            library_server: library_server.to_owned(),
            user_id: user.id,
        };
        self.store
            .open_session(&SessionId::of_token(token.expose()), &owner, &api_key)
            .await
            .map_err(|error| {
                tracing::error!(
                    operation = "open a session",
                    library_server = owner.library_server,
                    user_id = owner.user_id,
                    %error,
                    "storage failed"
                );
                ApiError::Internal
            })?;
        Ok(Session {
            token,
            user: CurrentUser {
                library_server: owner.library_server,
                user_id: owner.user_id,
                username: user.username,
            },
        })
    }

    async fn logout(&self, token: SessionToken) -> Result<(), ApiError> {
        let closed = self
            .store
            .close_session(&SessionId::of_token(token.expose()))
            .await
            .map_err(storage_error("close a session"))?;
        if closed {
            Ok(())
        } else {
            Err(ApiError::SessionInvalid)
        }
    }

    /// Asks the session's Library Server who its key belongs to. A key the Library Server
    /// rejects, or that now belongs to another user, is revoked, ending this user's sessions
    /// opened with it.
    async fn current_user(&self, token: SessionToken) -> Result<CurrentUser, ApiError> {
        let session = self
            .store
            .session(&SessionId::of_token(token.expose()))
            .await
            .map_err(storage_error("look up a session"))?
            .ok_or(ApiError::SessionInvalid)?;
        // The operator may have removed the Library Server since the login.
        let library_server = self
            .configured_base_url(&session.user.library_server)
            .ok_or_else(|| {
                tracing::info!(
                    library_server = session.user.library_server,
                    "session call for a Library Server that is not configured"
                );
                ApiError::SessionInvalid
            })?;
        match self.abs.me(library_server, &session.api_key).await {
            Ok(user) if user.id == session.user.user_id => Ok(CurrentUser {
                library_server: library_server.to_owned(),
                user_id: user.id,
                username: user.username,
            }),
            // An admin can move a key to another user; the session must not follow it.
            Ok(_) => Err(self
                .revoke(&session, "the API key now acts for another user")
                .await),
            Err(abs::Error::Rejected) => Err(self
                .revoke(&session, "the Library Server rejected the API key")
                .await),
            Err(error) => Err(abs_error(library_server, error)),
        }
    }

    /// Revokes the session's key for its user because of `reason`, ending this user's sessions
    /// opened with it. Returns the error the call answers with.
    async fn revoke(&self, session: &store::Session, reason: &str) -> ApiError {
        let (library_server, user_id) = (&session.user.library_server, &session.user.user_id);
        match self
            .store
            .revoke_api_key(&session.user, &session.api_key)
            .await
        {
            Ok(revoked) => {
                // A concurrent call may have revoked it first; that one logged it.
                if revoked {
                    tracing::info!(
                        library_server,
                        user_id,
                        reason,
                        "API key revoked, this user's sessions opened with it ended"
                    );
                }
                ApiError::ApiKeyRejected
            }
            Err(error) => {
                tracing::error!(
                    operation = "revoke an API key",
                    library_server,
                    user_id,
                    reason,
                    %error,
                    "storage failed"
                );
                ApiError::Internal
            }
        }
    }
}

impl<S: Store> Bridge for AppApi<S> {
    fn library_servers(&self) -> Vec<LibraryServer> {
        self.library_servers.clone()
    }

    fn login(
        &self,
        library_server: String,
        api_key: api::ApiKey,
    ) -> BoxFuture<'_, Result<Session, ApiError>> {
        Box::pin(self.login(library_server, api_key))
    }

    fn logout(&self, token: SessionToken) -> BoxFuture<'_, Result<(), ApiError>> {
        Box::pin(self.logout(token))
    }

    fn current_user(&self, token: SessionToken) -> BoxFuture<'_, Result<CurrentUser, ApiError>> {
        Box::pin(self.current_user(token))
    }
}

/// 256 random bits from the operating system, hex-encoded.
fn new_session_token() -> Result<SessionToken, ApiError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| {
        tracing::error!(%error, "no randomness for a session token");
        ApiError::Internal
    })?;
    let mut token = String::with_capacity(2 * bytes.len());
    for byte in bytes {
        write!(token, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(SessionToken::new(token))
}

fn abs_error(library_server: &str, error: abs::Error) -> ApiError {
    match error {
        abs::Error::Rejected => ApiError::ApiKeyRejected,
        abs::Error::Unavailable(_) => {
            tracing::warn!(library_server, %error, "Library Server unavailable");
            ApiError::LibraryServerUnreachable
        }
    }
}

/// Logs a storage failure during `operation` and hides it from the App.
fn storage_error(operation: &'static str) -> impl FnOnce(store::Error) -> ApiError {
    move |error| {
        tracing::error!(operation, %error, "storage failed");
        ApiError::Internal
    }
}
