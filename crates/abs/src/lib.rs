//! The Audiobookshelf client: the Bridge's only way to talk to a Library Server.
//!
//! It covers what the Bridge uses so far: the "who am I" check (`GET /api/me`) that proves an
//! API key (ADR 0011). Every request authenticates with `Authorization: Bearer <api key>`.

use std::fmt;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{StatusCode, redirect};
use serde::Deserialize;

/// How long one request to a Library Server may take, connecting included, before it counts
/// as unreachable.
const TIMEOUT: Duration = Duration::from_secs(10);

/// An Audiobookshelf API key. It never shows in `Debug` output, so it cannot leak into logs.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The key itself, for sending it to its Library Server or storing it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(REDACTED)")
    }
}

/// The Audiobookshelf user an API key acts for.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    /// Audiobookshelf's id for the user, stable across renames.
    pub id: String,
    pub username: String,
}

/// Why a request to a Library Server failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The Library Server does not accept the API key (HTTP 401): unknown, deleted,
    /// deactivated or expired, or its user is deactivated.
    #[error("the Library Server rejected the API key")]
    Rejected,
    /// No usable answer: the Library Server could not be reached in time, or answered with
    /// something other than success or a rejection (a 403, which may come from a proxy in
    /// front of it, a server error, a redirect, a body that is not what Audiobookshelf sends).
    /// The message describes the cause in full, without the URL.
    #[error("the Library Server gave no usable answer: {0}")]
    Unavailable(String),
}

impl Error {
    /// `Unavailable`, described by `error` and every error that caused it.
    fn unavailable(error: reqwest::Error) -> Self {
        let error = error.without_url();
        let mut message = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            message.push_str(": ");
            message.push_str(&cause.to_string());
            source = cause.source();
        }
        Self::Unavailable(message)
    }
}

/// A client for any number of Library Servers.
#[derive(Debug)]
pub struct Client {
    http: reqwest::Client,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            // reqwest drops `Authorization` on a redirect to another origin, but would send the
            // key again to wherever the same origin points; an API call has no reason to move.
            .redirect(redirect::Policy::none())
            .build()
            .expect("the TLS backend and system configuration load");
        Self { http }
    }

    /// The user `api_key` acts for on the Library Server at `base_url` (a normalized base URL
    /// without a trailing slash, ADR 0006).
    pub async fn me(&self, base_url: &str, api_key: &ApiKey) -> Result<User, Error> {
        // A key that cannot be sent as a header value cannot be a valid key.
        let mut authorization = HeaderValue::try_from(format!("Bearer {}", api_key.expose()))
            .map_err(|_| Error::Rejected)?;
        authorization.set_sensitive(true);

        let response = self
            .http
            .get(format!("{base_url}/api/me"))
            .header(AUTHORIZATION, authorization)
            .send()
            .await
            .map_err(Error::unavailable)?;
        match response.status() {
            StatusCode::UNAUTHORIZED => Err(Error::Rejected),
            status if status.is_success() => response.json().await.map_err(Error::unavailable),
            status => Err(Error::Unavailable(format!("HTTP {status}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Answers a real Audiobookshelf never gives on its own, so the black-box tests cannot
    //! provoke them.

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Redirect};
    use axum::routing::get;
    use tokio::net::TcpListener;

    use super::{ApiKey, Client, Error, User};

    async fn serve(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    async fn me_from(
        answer: impl IntoResponse + Clone + Send + Sync + 'static,
    ) -> Result<User, Error> {
        let base_url = serve(Router::new().route(
            "/api/me",
            get(move || {
                let answer = answer.clone();
                async move { answer }
            }),
        ))
        .await;
        Client::new().me(&base_url, &ApiKey::new("key")).await
    }

    #[tokio::test]
    async fn sends_the_key_as_a_bearer_token_under_the_base_path() {
        let base_url = serve(Router::new().route(
            "/abs/api/me",
            get(|headers: HeaderMap| async move {
                if headers["authorization"] == "Bearer s3cret" {
                    (
                        StatusCode::OK,
                        r#"{"id":"u1","username":"alice","type":"user"}"#,
                    )
                } else {
                    (StatusCode::UNAUTHORIZED, "Unauthorized")
                }
            }),
        ))
        .await;

        let user = Client::new()
            .me(&format!("{base_url}/abs"), &ApiKey::new("s3cret"))
            .await
            .unwrap();
        assert_eq!(
            user,
            User {
                id: "u1".into(),
                username: "alice".into()
            }
        );
    }

    #[tokio::test]
    async fn unauthorized_is_a_rejection() {
        assert!(matches!(
            me_from(StatusCode::UNAUTHORIZED).await,
            Err(Error::Rejected)
        ));
    }

    /// A proxy or auth wall in front of the Library Server answers 403 too; that must not
    /// count as the key being revoked.
    #[tokio::test]
    async fn forbidden_is_unavailable_not_a_rejection() {
        assert!(matches!(
            me_from(StatusCode::FORBIDDEN).await,
            Err(Error::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn an_unreachable_server_is_described_down_to_the_cause() {
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let Err(Error::Unavailable(message)) = Client::new()
            .me(&format!("http://{closed}"), &ApiKey::new("s3cret"))
            .await
        else {
            panic!("expected Unavailable");
        };
        assert!(message.to_lowercase().contains("refused"), "{message}");
        assert!(!message.contains(&closed.to_string()), "{message}");
        assert!(!message.contains("s3cret"), "{message}");
    }

    #[tokio::test]
    async fn a_server_error_is_unavailable() {
        assert!(matches!(
            me_from(StatusCode::INTERNAL_SERVER_ERROR).await,
            Err(Error::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed() {
        assert!(matches!(
            me_from(Redirect::temporary("http://elsewhere.invalid/api/me")).await,
            Err(Error::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn a_body_that_is_not_a_user_is_unavailable() {
        assert!(matches!(
            me_from("<html>a proxy's login page</html>").await,
            Err(Error::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn a_key_that_cannot_be_a_header_is_rejected_without_a_request() {
        let error = Client::new()
            .me("http://127.0.0.1:9", &ApiKey::new("line\nbreak"))
            .await;
        assert!(matches!(error, Err(Error::Rejected)));
    }

    #[test]
    fn debug_output_hides_the_key() {
        assert_eq!(format!("{:?}", ApiKey::new("s3cret")), "ApiKey(REDACTED)");
    }
}
