//! The App API's view of this Bridge: configured Library Servers, logins and sessions, and
//! browsing a session's Library Server as its user.

use std::fmt::Write;

use api::server::{BoxFuture, Bridge};
use api::{
    ApiError, CurrentUser, EpisodeFilter, Item, ItemId, Library, LibraryEntry, LibraryServer,
    Podcast, Session, SessionToken,
};
use store::{LibraryUser, SessionId, Store};

use crate::browse;
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
            .map_err(|error| abs_error(library_server, Request::ME, error))?;

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
                    error = crate::error_chain(&error),
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

    /// Asks the session's Library Server who its key belongs to.
    async fn current_user(&self, token: SessionToken) -> Result<CurrentUser, ApiError> {
        let (library_server, session) = self.session(&token).await?;
        let user = self.abs.me(library_server, &session.api_key).await;
        let user = self.verified(library_server, &session, user).await?;
        Ok(CurrentUser {
            library_server: library_server.to_owned(),
            user_id: user.id,
            username: user.username,
        })
    }

    async fn libraries(&self, token: SessionToken) -> Result<Vec<Library>, ApiError> {
        let request = Request {
            operation: "list the Libraries",
            id: None,
        };
        let (_, libraries) = self
            .browse(token, request, async |server, key| {
                self.abs.libraries(server, key).await
            })
            .await?;
        Ok(libraries.into_iter().map(browse::library).collect())
    }

    async fn library_entries(
        &self,
        token: SessionToken,
        library: String,
    ) -> Result<Vec<LibraryEntry>, ApiError> {
        let request = Request {
            operation: "list a Library",
            id: Some(&library),
        };
        let (_, items) = self
            .browse(token, request, async |server, key| {
                self.abs.library_items(server, key, &library).await
            })
            .await?;
        Ok(items.into_iter().map(browse::library_entry).collect())
    }

    async fn podcast(
        &self,
        token: SessionToken,
        podcast: String,
        filter: EpisodeFilter,
    ) -> Result<Podcast, ApiError> {
        let request = Request {
            operation: "fetch a podcast",
            id: Some(&podcast),
        };
        let (user, item) = self
            .browse(token, request, async |server, key| {
                self.abs.library_item(server, key, &podcast).await
            })
            .await?;
        browse::podcast(&user, item, filter)
    }

    async fn item(&self, token: SessionToken, id: ItemId) -> Result<Item, ApiError> {
        let request = Request {
            operation: "fetch an Item",
            id: Some(&id.library_item),
        };
        let (user, library_item) = self
            .browse(token, request, async |server, key| {
                self.abs.library_item(server, key, &id.library_item).await
            })
            .await?;
        browse::item(&user, library_item, id)
    }

    /// The session `token` names, and the configured Library Server it belongs to.
    async fn session(&self, token: &SessionToken) -> Result<(&str, store::Session), ApiError> {
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
        Ok((library_server, session))
    }

    /// Fetches something with `fetch` (the `request`) from the session's Library Server as its
    /// user, and checks at the same time that the session's key still acts for that user.
    /// Returns the user, with their Progress, and what was fetched.
    async fn browse<T>(
        &self,
        token: SessionToken,
        request: Request<'_>,
        fetch: impl AsyncFnOnce(&str, &abs::ApiKey) -> Result<T, abs::Error>,
    ) -> Result<(abs::User, T), ApiError> {
        let (library_server, session) = self.session(&token).await?;
        let (user, fetched) = tokio::join!(
            self.abs.me(library_server, &session.api_key),
            fetch(library_server, &session.api_key),
        );
        let user = self.verified(library_server, &session, user).await?;
        match fetched {
            Ok(fetched) => Ok((user, fetched)),
            Err(error) => Err(self.failure(library_server, &session, request, error).await),
        }
    }

    /// The session's user as the Library Server described them (`me`), if its key still acts
    /// for them. A key the Library Server rejects, or that now belongs to another user, is
    /// revoked, ending this user's sessions opened with it.
    async fn verified(
        &self,
        library_server: &str,
        session: &store::Session,
        me: Result<abs::User, abs::Error>,
    ) -> Result<abs::User, ApiError> {
        match me {
            Ok(user) if user.id == session.user.user_id => Ok(user),
            // An admin can move a key to another user; the session must not follow it.
            Ok(_) => Err(self
                .revoke(session, "the API key now acts for another user")
                .await),
            Err(error) => Err(self
                .failure(library_server, session, Request::ME, error)
                .await),
        }
    }

    /// The error a session call answers with when its Library Server failed `request` with
    /// `error`. A rejected key is revoked.
    async fn failure(
        &self,
        library_server: &str,
        session: &store::Session,
        request: Request<'_>,
        error: abs::Error,
    ) -> ApiError {
        match error {
            abs::Error::Rejected => {
                self.revoke(session, "the Library Server rejected the API key")
                    .await
            }
            error => abs_error(library_server, request, error),
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
                    error = crate::error_chain(&error),
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

    fn libraries(&self, token: SessionToken) -> BoxFuture<'_, Result<Vec<Library>, ApiError>> {
        Box::pin(self.libraries(token))
    }

    fn library_entries(
        &self,
        token: SessionToken,
        library: String,
    ) -> BoxFuture<'_, Result<Vec<LibraryEntry>, ApiError>> {
        Box::pin(self.library_entries(token, library))
    }

    fn item(&self, token: SessionToken, id: ItemId) -> BoxFuture<'_, Result<Item, ApiError>> {
        Box::pin(self.item(token, id))
    }

    fn podcast(
        &self,
        token: SessionToken,
        podcast: String,
        filter: EpisodeFilter,
    ) -> BoxFuture<'_, Result<Podcast, ApiError>> {
        Box::pin(self.podcast(token, podcast, filter))
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

/// What the Bridge asked a Library Server for, for its log events.
#[derive(Debug, Clone, Copy)]
struct Request<'a> {
    operation: &'static str,
    /// The Library or library item asked for, if any.
    id: Option<&'a str>,
}

impl Request<'_> {
    /// The "who am I" check that proves an API key.
    const ME: Request<'static> = Request {
        operation: "check who the API key acts for",
        id: None,
    };
}

fn abs_error(library_server: &str, request: Request, error: abs::Error) -> ApiError {
    match error {
        abs::Error::Rejected => ApiError::ApiKeyRejected,
        abs::Error::NotFound => ApiError::NotFound,
        abs::Error::Unavailable(_) => {
            tracing::warn!(
                library_server,
                operation = request.operation,
                id = request.id,
                %error,
                "Library Server unavailable"
            );
            ApiError::LibraryServerUnreachable
        }
    }
}

/// Logs a storage failure during `operation` and hides it from the App.
fn storage_error(operation: &'static str) -> impl FnOnce(store::Error) -> ApiError {
    move |error| {
        tracing::error!(
            operation,
            error = crate::error_chain(&error),
            "storage failed"
        );
        ApiError::Internal
    }
}
