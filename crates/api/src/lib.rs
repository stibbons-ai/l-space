//! The App API: the contract between the App and the Bridge.
//!
//! The contract is shared Rust types plus Dioxus server functions, so neither side generates
//! code from the other. With the `server` feature (the Bridge) a server function runs its body
//! and `server::router` serves it over HTTP; without it (the App) calling the same function
//! sends the HTTP request to the Bridge.
//!
//! Errors the App shows to a person are [`ApiError`] codes, which the App translates. The
//! Bridge never sends prose for the App to display.
//!
//! A person logs in with [`login`] and gets a [`Session`]. Every call that acts for them sends
//! its token as `Authorization: Bearer <token>`; on the App's side, set it for every server
//! function with `dioxus::fullstack::set_request_headers`.

use std::fmt;

use dioxus::fullstack::{AsStatusCode, ServerFnError, StatusCode};
#[cfg(feature = "server")]
use dioxus::prelude::dioxus_server;
use dioxus::prelude::{dioxus_fullstack, get, post};
use serde::{Deserialize, Serialize};

/// A Library Server the operator configured on the Bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryServer {
    /// The normalized base URL, which identifies the Library Server (ADR 0006).
    pub base_url: String,
    /// The operator's display name for it, if any.
    pub name: Option<String>,
}

/// A logged-in App session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Authenticates the session's calls. It survives Bridge restarts and lasts until
    /// [`logout`], or until the Library Server rejects the API key the session was opened with
    /// (or the key now acts for another user). While the operator has removed the Library
    /// Server from the configuration the session is unusable; re-adding it makes the session
    /// usable again (the Library Server still checks the key on every call).
    pub token: SessionToken,
    pub user: CurrentUser,
}

/// The secret that authenticates an App session. It never shows in `Debug` output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionToken(String);

impl SessionToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(REDACTED)")
    }
}

/// An Audiobookshelf API key as the App sends it to log in. It never shows in `Debug`
/// output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(REDACTED)")
    }
}

/// The Audiobookshelf user a session acts for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentUser {
    /// The normalized base URL of the user's Library Server.
    pub library_server: String,
    /// Audiobookshelf's id for the user.
    pub user_id: String,
    pub username: String,
}

/// An error code the App translates for the user.
///
/// On the wire this is the `data` member of the error body, for example
/// `{"code":"internal"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ApiError {
    /// The App could not reach the Bridge, or could not understand its answer. Produced only
    /// on the App's side, by the client half of a server function; the Bridge never sends it.
    #[error("bridge_unreachable")]
    BridgeUnreachable,
    /// The Bridge failed in a way the user cannot fix.
    #[error("internal")]
    Internal,
    /// The Library Server rejected the API key: it is wrong, or was deleted, deactivated or
    /// expired, or its user was deactivated; or the key now acts for another user than the
    /// session's. On a session call, this user's sessions opened with the key have ended; a
    /// rejected login ends nothing.
    #[error("api_key_rejected")]
    ApiKeyRejected,
    /// The Library Server is not one the operator configured on this Bridge.
    #[error("library_server_not_configured")]
    LibraryServerNotConfigured,
    /// The Library Server could not be reached, or gave no usable answer.
    #[error("library_server_unreachable")]
    LibraryServerUnreachable,
    /// The call needs a session and has none: no token, an unknown one, one whose session
    /// ended, or one whose Library Server is not configured (any more). The App must log in
    /// again.
    #[error("session_invalid")]
    SessionInvalid,
    /// A code this build of the App does not know yet, from a newer Bridge. Produced only on
    /// the App's side, when decoding; the Bridge never sends it.
    #[serde(other)]
    #[error("unknown")]
    Unknown,
}

impl From<ServerFnError> for ApiError {
    fn from(error: ServerFnError) -> Self {
        match error {
            ServerFnError::Request(_)
            | ServerFnError::StreamError(_)
            | ServerFnError::Deserialization(_) => Self::BridgeUnreachable,
            _ => Self::Internal,
        }
    }
}

impl AsStatusCode for ApiError {
    fn as_status_code(&self) -> StatusCode {
        match self {
            Self::ApiKeyRejected | Self::SessionInvalid => StatusCode::UNAUTHORIZED,
            Self::LibraryServerNotConfigured => StatusCode::NOT_FOUND,
            Self::BridgeUnreachable | Self::LibraryServerUnreachable => StatusCode::BAD_GATEWAY,
            Self::Internal | Self::Unknown => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// Lists the Library Servers configured on the Bridge, in configuration order.
#[get("/api/library-servers", bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>)]
pub async fn library_servers() -> Result<Vec<LibraryServer>, ApiError> {
    Ok(bridge.library_servers())
}

/// Logs in to the Library Server at `library_server` (its base URL; any spelling that
/// normalizes to a configured one) with an Audiobookshelf API key.
#[post("/api/login", bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>)]
pub async fn login(library_server: String, api_key: ApiKey) -> Result<Session, ApiError> {
    bridge.login(library_server, api_key).await
}

/// Ends the caller's session.
#[post(
    "/api/logout",
    headers: dioxus_fullstack::HeaderMap,
    bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>
)]
pub async fn logout() -> Result<(), ApiError> {
    bridge.logout(server::session_token(&headers)?).await
}

/// The Audiobookshelf user the caller's session acts for, as its Library Server sees them now.
#[get(
    "/api/me",
    headers: dioxus_fullstack::HeaderMap,
    bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>
)]
pub async fn current_user() -> Result<CurrentUser, ApiError> {
    bridge.current_user(server::session_token(&headers)?).await
}

/// The Bridge's side of the App API.
#[cfg(feature = "server")]
pub mod server {
    use std::pin::Pin;
    use std::sync::Arc;

    use dioxus::fullstack::HeaderMap;
    use dioxus::fullstack::axum::{Extension, Router};
    use dioxus::fullstack::headers::HeaderMapExt;
    use dioxus::fullstack::headers::authorization::{Authorization, Bearer};
    use dioxus::server::{DioxusRouterExt, FullstackState};

    use crate::{ApiError, ApiKey, CurrentUser, LibraryServer, Session, SessionToken};

    /// A boxed future, so [`Bridge`] stays object-safe.
    pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

    /// What the App API needs from the Bridge. The `bridge` binary implements it.
    pub trait Bridge: Send + Sync + 'static {
        /// The configured Library Servers, in configuration order.
        fn library_servers(&self) -> Vec<LibraryServer>;

        /// See [`crate::login`].
        fn login(
            &self,
            library_server: String,
            api_key: ApiKey,
        ) -> BoxFuture<'_, Result<Session, ApiError>>;

        /// See [`crate::logout`].
        fn logout(&self, token: SessionToken) -> BoxFuture<'_, Result<(), ApiError>>;

        /// See [`crate::current_user`].
        fn current_user(&self, token: SessionToken)
        -> BoxFuture<'_, Result<CurrentUser, ApiError>>;
    }

    /// How server functions reach the [`Bridge`].
    pub type BridgeHandle = Arc<dyn Bridge>;

    /// The one place a key from the App becomes the key the Bridge sends to Audiobookshelf.
    impl From<ApiKey> for abs::ApiKey {
        fn from(key: ApiKey) -> Self {
            Self::new(key.0)
        }
    }

    /// An axum router serving every App API server function, backed by `bridge`.
    pub fn router(bridge: BridgeHandle) -> Router {
        Router::new()
            .register_server_functions()
            .layer(Extension(bridge))
            .with_state(FullstackState::headless())
    }

    /// The session token from `Authorization: Bearer <token>`.
    pub(crate) fn session_token(headers: &HeaderMap) -> Result<SessionToken, ApiError> {
        let authorization = headers
            .typed_get::<Authorization<Bearer>>()
            .ok_or(ApiError::SessionInvalid)?;
        match authorization.token().trim() {
            "" => Err(ApiError::SessionInvalid),
            token => Ok(SessionToken::new(token)),
        }
    }
}

#[cfg(test)]
mod tests {
    use dioxus::fullstack::{AsStatusCode, StatusCode};
    use serde_json::json;

    use super::{ApiError, ApiKey, SessionToken};

    #[test]
    fn error_codes_travel_as_a_code_field() {
        assert_eq!(
            serde_json::to_value(ApiError::Internal).unwrap(),
            json!({"code": "internal"})
        );
        assert_eq!(
            serde_json::from_value::<ApiError>(json!({"code": "internal"})).unwrap(),
            ApiError::Internal
        );
    }

    #[test]
    fn a_code_from_a_newer_bridge_decodes_as_unknown() {
        assert_eq!(
            serde_json::from_value::<ApiError>(json!({"code": "group_busy"})).unwrap(),
            ApiError::Unknown
        );
    }

    #[test]
    fn unknown_encodes_as_a_server_error() {
        assert_eq!(
            serde_json::to_value(ApiError::Unknown).unwrap(),
            json!({"code": "unknown"})
        );
        assert_eq!(
            ApiError::Unknown.as_status_code(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn debug_output_hides_secrets() {
        assert_eq!(
            format!("{:?}", SessionToken::new("s3cret")),
            "SessionToken(REDACTED)"
        );
        assert_eq!(format!("{:?}", ApiKey::new("s3cret")), "ApiKey(REDACTED)");
    }

    #[cfg(feature = "server")]
    mod server {
        use dioxus::fullstack::HeaderMap;
        use dioxus::fullstack::http::header::AUTHORIZATION;

        use crate::ApiError;
        use crate::server::session_token;

        fn token_from(authorization: Option<&str>) -> Result<String, ApiError> {
            let mut headers = HeaderMap::new();
            if let Some(value) = authorization {
                headers.insert(AUTHORIZATION, value.parse().unwrap());
            }
            session_token(&headers).map(|token| token.expose().to_owned())
        }

        #[test]
        fn takes_the_token_from_a_bearer_authorization() {
            assert_eq!(token_from(Some("Bearer abc")).unwrap(), "abc");
            assert_eq!(token_from(Some("bearer  abc ")).unwrap(), "abc");
        }

        #[test]
        fn anything_else_is_an_invalid_session() {
            for authorization in [
                None,
                Some("Basic abc"),
                Some("Bearer"),
                Some("Bearer  "),
                Some("abc"),
            ] {
                assert_eq!(
                    token_from(authorization),
                    Err(ApiError::SessionInvalid),
                    "{authorization:?}"
                );
            }
        }
    }
}
