//! The Audiobookshelf client: the Bridge's only way to talk to a Library Server.
//!
//! It covers the "who am I" check (`GET /api/me`) that proves an API key (ADR 0011) and also
//! brings the user's Progress, and read-only browsing: the user's Libraries, a Library's
//! audiobooks or podcasts, and one audiobook or podcast with its chapters, audio files and
//! episodes. Every request authenticates with `Authorization: Bearer <api key>`.
//!
//! Ids a caller passes go into request paths only if they are plain Audiobookshelf ids (ASCII
//! letters, digits, `-` and `_`); any other id names nothing ([`Error::NotFound`]) and is
//! never sent.

use std::fmt;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{StatusCode, redirect};
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// How long one request to a Library Server may take, connecting included, before it counts
/// as unreachable.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How many audiobooks or podcasts one request for a Library's contents asks for.
const PAGE_SIZE: usize = 100;

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

/// The Audiobookshelf user an API key acts for, with their Progress.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    /// Audiobookshelf's id for the user, stable across renames.
    pub id: String,
    pub username: String,
    /// The user's Progress, one entry per audiobook or episode they started or Finished.
    #[serde(default)]
    pub media_progress: Vec<MediaProgress>,
}

impl User {
    /// The user's Progress in the audiobook `library_item_id`, or in its episode `episode_id`.
    pub fn progress(
        &self,
        library_item_id: &str,
        episode_id: Option<&str>,
    ) -> Option<&MediaProgress> {
        self.media_progress.iter().find(|progress| {
            progress.library_item_id == library_item_id
                && progress.episode_id.as_deref() == episode_id
        })
    }
}

/// A user's Progress in one audiobook or episode.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaProgress {
    pub library_item_id: String,
    /// The episode, for a podcast's.
    pub episode_id: Option<String>,
    /// Seconds into the audiobook or episode.
    #[serde(default)]
    pub current_time: f64,
    #[serde(default)]
    pub is_finished: bool,
}

/// A Library: audiobooks or podcasts.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Library {
    pub id: String,
    pub name: String,
    pub media_type: MediaType,
}

/// What a Library holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Book,
    Podcast,
}

/// An entry in a Library: an audiobook or a podcast. Listed in a Library, only the metadata
/// is filled in; [`Client::library_item`] brings the rest.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LibraryItem {
    pub id: String,
    #[serde(flatten)]
    pub media: Media,
}

/// An audiobook or a podcast.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "mediaType", content = "media", rename_all = "lowercase")]
pub enum Media {
    Book(Book),
    Podcast(Podcast),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Book {
    pub metadata: BookMetadata,
    /// Seconds, all audio files together.
    #[serde(default)]
    pub duration: f64,
    #[serde(default)]
    pub chapters: Vec<Chapter>,
    /// The audio files in playing order.
    #[serde(default)]
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookMetadata {
    pub title: String,
    pub author_name: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Podcast {
    pub metadata: PodcastMetadata,
    #[serde(default)]
    pub episodes: Vec<Episode>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PodcastMetadata {
    pub title: String,
    pub author: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Episode {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub published_at: Option<i64>,
    /// Seconds.
    pub duration: Option<f64>,
    pub audio_track: Option<Track>,
}

/// A chapter; times in seconds into the whole audiobook.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Chapter {
    pub title: String,
    pub start: f64,
    pub end: f64,
}

/// An audio file as part of an audiobook or episode.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    /// Audiobookshelf's id for the file within its library item.
    pub ino: String,
    /// Seconds into the whole audiobook where this file starts.
    pub start_offset: f64,
    /// Seconds.
    pub duration: f64,
}

/// Why a request to a Library Server failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The Library Server does not accept the API key (HTTP 401): unknown, deleted,
    /// deactivated or expired, or its user is deactivated. Also a key that cannot be sent as
    /// a header value, which is rejected without a request.
    #[error("the API key was rejected")]
    Rejected,
    /// No usable answer: the Library Server could not be reached in time, or answered with
    /// something other than success, a rejection or a 404 (a 403, which may come from a proxy
    /// in front of it, a server error, a redirect, a body that is not what Audiobookshelf
    /// sends). The message describes the cause in full, without the URL; a cause may still
    /// name the host (a TLS certificate that doesn't match it, for example).
    #[error("the Library Server gave no usable answer: {0}")]
    Unavailable(String),
    /// The Library Server has no Library or library item with this id for the user (HTTP
    /// 404), or the id is not an Audiobookshelf id.
    #[error("no such Library or library item")]
    NotFound,
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
            // reqwest drops `Authorization` on a redirect to another host or port, but would
            // send the key along on a redirect to the same host and port, even over plain
            // http; an API call has no reason to move.
            .redirect(redirect::Policy::none())
            .build()
            .expect("the TLS backend and system configuration load");
        Self { http }
    }

    /// The user `api_key` acts for on the Library Server at `base_url` (a normalized base URL
    /// without a trailing slash, ADR 0006), with their Progress.
    pub async fn me(&self, base_url: &str, api_key: &ApiKey) -> Result<User, Error> {
        let user: User = self
            .get(base_url, api_key, "/api/me")
            .await
            // Every Audiobookshelf has this endpoint: whatever answered is something else.
            .map_err(|error| match error {
                Error::NotFound => Error::Unavailable("HTTP 404 Not Found".to_owned()),
                error => error,
            })?;
        if user.id.is_empty() {
            return Err(Error::Unavailable("the answer names no user id".to_owned()));
        }
        Ok(user)
    }

    /// The Libraries the user can see, in the Library Server's order.
    pub async fn libraries(&self, base_url: &str, api_key: &ApiKey) -> Result<Vec<Library>, Error> {
        #[derive(Deserialize)]
        struct Libraries {
            libraries: Vec<Library>,
        }
        let libraries: Libraries = self.get(base_url, api_key, "/api/libraries").await?;
        Ok(libraries.libraries)
    }

    /// Every audiobook or podcast in Library `library_id`, by title, with metadata only.
    pub async fn library_items(
        &self,
        base_url: &str,
        api_key: &ApiKey,
        library_id: &str,
    ) -> Result<Vec<LibraryItem>, Error> {
        #[derive(Deserialize)]
        struct Page {
            results: Vec<LibraryItem>,
            total: usize,
        }
        let library_id = id_segment(library_id)?;
        let mut items = Vec::new();
        for page in 0.. {
            let path = format!(
                "/api/libraries/{library_id}/items?minified=1&sort=media.metadata.title\
                 &limit={PAGE_SIZE}&page={page}"
            );
            let Page { results, total } = self.get(base_url, api_key, &path).await?;
            let last = results.is_empty();
            items.extend(results);
            if last || items.len() >= total {
                break;
            }
        }
        Ok(items)
    }

    /// The audiobook or podcast `library_item_id` in full: chapters and audio files, or
    /// episodes.
    pub async fn library_item(
        &self,
        base_url: &str,
        api_key: &ApiKey,
        library_item_id: &str,
    ) -> Result<LibraryItem, Error> {
        let library_item_id = id_segment(library_item_id)?;
        self.get(
            base_url,
            api_key,
            &format!("/api/items/{library_item_id}?expanded=1"),
        )
        .await
    }

    /// GETs `path_and_query` and decodes the answer.
    async fn get<T: DeserializeOwned>(
        &self,
        base_url: &str,
        api_key: &ApiKey,
        path_and_query: &str,
    ) -> Result<T, Error> {
        // A key that cannot be sent as a header value cannot be a valid key.
        let mut authorization = HeaderValue::try_from(format!("Bearer {}", api_key.expose()))
            .map_err(|_| Error::Rejected)?;
        authorization.set_sensitive(true);

        let response = self
            .http
            .get(format!("{base_url}{path_and_query}"))
            .header(AUTHORIZATION, authorization)
            .send()
            .await
            .map_err(Error::unavailable)?;
        match response.status() {
            StatusCode::UNAUTHORIZED => Err(Error::Rejected),
            StatusCode::NOT_FOUND => Err(Error::NotFound),
            status if status.is_success() => response.json().await.map_err(Error::unavailable),
            status => Err(Error::Unavailable(format!("HTTP {status}"))),
        }
    }
}

/// `id` if it is an Audiobookshelf id, which goes into a path as it is.
fn id_segment(id: &str) -> Result<&str, Error> {
    let plain = !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if plain { Ok(id) } else { Err(Error::NotFound) }
}

#[cfg(test)]
mod tests {
    //! Answers a real Audiobookshelf never gives on its own, so the black-box tests cannot
    //! provoke them.

    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::Query;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Redirect};
    use axum::routing::get;
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::{ApiKey, Client, Error, Media, User};

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
                username: "alice".into(),
                media_progress: Vec::new(),
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
    async fn a_user_without_an_id_is_unavailable() {
        assert!(matches!(
            me_from(r#"{"id":"","username":"x"}"#).await,
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

    /// A Library of `total` books, served in the pages asked for; counts the requests.
    async fn library_of(total: usize, requests: Arc<AtomicUsize>) -> String {
        serve(Router::new().route(
            "/api/libraries/lib-1/items",
            get(move |Query(query): Query<HashMap<String, String>>| {
                let requests = requests.clone();
                async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    let limit: usize = query["limit"].parse().unwrap();
                    let page: usize = query["page"].parse().unwrap();
                    let results: Vec<_> = (page * limit..total.min((page + 1) * limit))
                        .map(|n| {
                            json!({
                                "id": format!("li-{n}"),
                                "mediaType": "book",
                                "media": {"metadata": {"title": format!("Book {n}"), "authorName": null}},
                            })
                        })
                        .collect();
                    axum::Json(json!({"results": results, "total": total, "limit": limit, "page": page}))
                }
            }),
        ))
        .await
    }

    #[tokio::test]
    async fn a_library_is_read_to_its_last_page() {
        let requests = Arc::new(AtomicUsize::new(0));
        let base_url = library_of(250, requests.clone()).await;

        let items = Client::new()
            .library_items(&base_url, &ApiKey::new("key"), "lib-1")
            .await
            .unwrap();

        let ids: Vec<_> = items.iter().map(|item| item.id.clone()).collect();
        let expected: Vec<_> = (0..250).map(|n| format!("li-{n}")).collect();
        assert_eq!(ids, expected);
        assert!(
            items
                .iter()
                .all(|item| matches!(item.media, Media::Book(_)))
        );
        assert_eq!(requests.load(Ordering::SeqCst), 3);
    }

    /// A total that overstates what the pages hold must not loop forever.
    #[tokio::test]
    async fn reading_a_library_stops_at_an_empty_page() {
        let base_url = serve(Router::new().route(
            "/api/libraries/lib-1/items",
            get(|| async { axum::Json(json!({"results": [], "total": 5})) }),
        ))
        .await;

        let items = Client::new()
            .library_items(&base_url, &ApiKey::new("key"), "lib-1")
            .await
            .unwrap();
        assert!(items.is_empty());
    }

    /// An id that could change the path (or the query) names nothing and is never sent: a
    /// request to the closed port would fail as unavailable instead.
    #[tokio::test]
    async fn an_id_that_is_not_an_audiobookshelf_id_is_not_found_without_a_request() {
        let client = Client::new();
        let key = ApiKey::new("key");
        for id in ["", "../me", "a/b", "a?b", "a#b", "a%2Fb", "a b", "ä", "."] {
            assert!(
                matches!(
                    client.library_item("http://127.0.0.1:9", &key, id).await,
                    Err(Error::NotFound)
                ),
                "{id:?}"
            );
            assert!(
                matches!(
                    client.library_items("http://127.0.0.1:9", &key, id).await,
                    Err(Error::NotFound)
                ),
                "{id:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_library_item_is_not_found() {
        let base_url = serve(Router::new()).await;
        assert!(matches!(
            Client::new()
                .library_item(&base_url, &ApiKey::new("key"), "li-1")
                .await,
            Err(Error::NotFound)
        ));
    }

    /// Every Audiobookshelf answers `/api/me`; a 404 there is something else.
    #[tokio::test]
    async fn a_missing_me_endpoint_is_unavailable() {
        assert!(matches!(
            me_from(StatusCode::NOT_FOUND).await,
            Err(Error::Unavailable(_))
        ));
    }
}
