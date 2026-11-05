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
use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Deserializer};

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

/// The Audiobookshelf user an API key acts for, with their Progress.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    /// Audiobookshelf's id for the user, stable across renames.
    pub id: String,
    pub username: String,
    /// The user's Progress: an entry per audiobook or episode Audiobookshelf holds Progress
    /// for (also at 0 seconds and not Finished).
    #[serde(default)]
    pub media_progress: Known<MediaProgress>,
}

/// A user's Progress in one audiobook or episode.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaProgress {
    /// The audiobook, or the podcast of the episode; Audiobookshelf leaves it out (null) for
    /// episode Progress written with only the episode's id.
    pub library_item_id: Option<String>,
    /// The episode, for a podcast's.
    pub episode_id: Option<String>,
    /// Seconds into the audiobook or episode.
    #[serde(default, deserialize_with = "null_as_default")]
    pub current_time: f64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub is_finished: bool,
}

/// A value Audiobookshelf may send as null, as its type's default.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// The entries of a list Audiobookshelf sent that decode as `T`, and how many others it left
/// out: one entry in a shape this client doesn't know must not fail the whole answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Known<T> {
    pub entries: Vec<T>,
    /// How many entries didn't decode.
    pub dropped: usize,
}

impl<T> Default for Known<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            dropped: 0,
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Known<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Entry<T> {
            Known(T),
            Unknown(IgnoredAny),
        }
        let mut known = Self::default();
        for entry in Option::<Vec<Entry<T>>>::deserialize(deserializer)?.unwrap_or_default() {
            match entry {
                Entry::Known(entry) => known.entries.push(entry),
                Entry::Unknown(_) => known.dropped += 1,
            }
        }
        Ok(known)
    }
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

/// An entry in a Library: an audiobook or a podcast (an ebook too, at this layer). Listed in
/// a Library, only the metadata, a book's `numTracks` and `duration` are filled in;
/// [`Client::library_item`] brings the rest.
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
#[serde(rename_all = "camelCase")]
pub struct Book {
    pub metadata: BookMetadata,
    /// Seconds, all audio files together; none when Audiobookshelf doesn't know it.
    #[serde(default, deserialize_with = "known_seconds")]
    pub duration: Option<f64>,
    /// How many audio files it has that aren't excluded; 0 for an ebook without audio.
    /// Unknown if Audiobookshelf leaves it out.
    #[serde(default)]
    pub num_tracks: Option<usize>,
    /// Its chapters; one in a shape this client doesn't know is left out.
    #[serde(default)]
    pub chapters: Known<Chapter>,
    /// The audio files in playing order.
    #[serde(default)]
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookMetadata {
    #[serde(default, deserialize_with = "null_as_default")]
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
    #[serde(default, deserialize_with = "null_as_default")]
    pub title: String,
    pub author: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Episode {
    pub id: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub title: String,
    pub description: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub published_at: Option<i64>,
    /// Seconds; none when Audiobookshelf doesn't know it.
    #[serde(default, deserialize_with = "known_seconds")]
    pub duration: Option<f64>,
    pub audio_track: Option<Track>,
}

/// A chapter; times in seconds into the whole audiobook.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Chapter {
    #[serde(default, deserialize_with = "null_as_default")]
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
    /// Seconds into the whole audiobook where this file starts, as Audiobookshelf adds up the
    /// durations of the files before it; an episode's file starts at 0.
    #[serde(default)]
    pub start_offset: Option<f64>,
    /// Seconds; none when Audiobookshelf could not probe the file.
    #[serde(default, deserialize_with = "known_seconds")]
    pub duration: Option<f64>,
}

/// A duration in seconds, unless Audiobookshelf doesn't know it: it then sends 0 (or null).
fn known_seconds<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<f64>, D::Error> {
    Ok(Option::<f64>::deserialize(deserializer)?
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0))
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
    /// something other than success, a rejection or a "not found", for example a server
    /// error, a redirect, a body that is not what Audiobookshelf sends, or a 403 or 404 on
    /// `/api/me` or `/api/libraries`, which every Audiobookshelf answers, so something in
    /// front of it did (a proxy, for example). The message describes the cause in full,
    /// without the URL; a cause may still name the host (a TLS certificate that doesn't match
    /// it, for example).
    #[error("the Library Server gave no usable answer: {0}")]
    Unavailable(String),
    /// The Library Server has no Library or library item with this id that the user may see.
    #[error("no such Library or library item ({0:?})")]
    NotFound(NotFound),
}

/// Why a Library or library item is not found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotFound {
    /// HTTP 403: Audiobookshelf answers this when the user may not see it.
    Forbidden,
    /// HTTP 404.
    Missing,
    /// The id is not an Audiobookshelf id; it was never sent.
    InvalidId,
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
            .get(base_url, api_key, "/api/me", Names::Nothing)
            .await?;
        if user.id.is_empty() {
            return Err(Error::Unavailable("the answer names no user id".to_owned()));
        }
        Ok(user)
    }

    /// The Libraries the user can see, in the Library Server's order; one in a shape this
    /// client doesn't know (another kind, for example) is left out and counted.
    pub async fn libraries(
        &self,
        base_url: &str,
        api_key: &ApiKey,
    ) -> Result<Known<Library>, Error> {
        #[derive(Deserialize)]
        struct Libraries {
            libraries: Known<Library>,
        }
        let libraries: Libraries = self
            .get(base_url, api_key, "/api/libraries", Names::Nothing)
            .await?;
        Ok(libraries.libraries)
    }

    /// Every audiobook or podcast in Library `library_id` (books without audio included), by
    /// title, with metadata, a book's `numTracks` and `duration` only.
    ///
    /// One request: without a `limit`, Audiobookshelf answers with all of them (2.37.2:
    /// `LibraryController.getLibraryItems` defaults `limit` to 0, which its item queries
    /// turn into no SQL `LIMIT`).
    pub async fn library_items(
        &self,
        base_url: &str,
        api_key: &ApiKey,
        library_id: &str,
    ) -> Result<Vec<LibraryItem>, Error> {
        #[derive(Deserialize)]
        struct Items {
            results: Vec<LibraryItem>,
        }
        let library_id = id_segment(library_id)?;
        let path =
            format!("/api/libraries/{library_id}/items?minified=1&sort=media.metadata.title");
        let items: Items = self.get(base_url, api_key, &path, Names::One).await?;
        Ok(items.results)
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
        let path = format!("/api/items/{library_item_id}?expanded=1");
        self.get(base_url, api_key, &path, Names::One).await
    }

    /// GETs `path_and_query`, which `names` a Library or library item or not, and decodes the
    /// answer.
    async fn get<T: DeserializeOwned>(
        &self,
        base_url: &str,
        api_key: &ApiKey,
        path_and_query: &str,
        names: Names,
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
        match (response.status(), names) {
            (StatusCode::UNAUTHORIZED, _) => Err(Error::Rejected),
            // Audiobookshelf answers 403 for a Library or item the user may not see.
            (StatusCode::FORBIDDEN, Names::One) => Err(Error::NotFound(NotFound::Forbidden)),
            (StatusCode::NOT_FOUND, Names::One) => Err(Error::NotFound(NotFound::Missing)),
            (status, _) if status.is_success() => response.json().await.map_err(Error::unavailable),
            // Any other answer, for example a server error, a redirect, or a 403/404 on
            // `/api/me` or `/api/libraries`, which every Audiobookshelf answers, so something
            // in front of it did.
            (status, _) => Err(Error::Unavailable(format!("HTTP {status}"))),
        }
    }
}

/// Whether a request names one Library or library item, which then may not exist for the user.
#[derive(Debug, Clone, Copy)]
enum Names {
    One,
    Nothing,
}

/// `id` if it is an Audiobookshelf id, which goes into a path as it is.
fn id_segment(id: &str) -> Result<&str, Error> {
    let plain = !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if plain {
        Ok(id)
    } else {
        Err(Error::NotFound(NotFound::InvalidId))
    }
}

#[cfg(test)]
mod tests {
    //! The client against small stand-ins for Audiobookshelf: answers a real one never gives
    //! on its own, and request details the black-box tests cannot see.

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Redirect};
    use axum::routing::get;
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::{
        ApiKey, Client, Error, Known, LibraryItem, Media, MediaProgress, MediaType, NotFound, User,
    };

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
                media_progress: Known::default(),
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

    /// Audiobookshelf writes episode Progress without its podcast when only the episode's id
    /// is given, and its columns allow nulls: such entries still decode, and one this client
    /// can't read is left out instead of failing the whole user.
    #[test]
    fn progress_with_nulls_decodes_and_an_unreadable_entry_is_left_out() {
        let user: User = serde_json::from_value(json!({
            "id": "u1",
            "username": "alice",
            "mediaProgress": [
                {"libraryItemId": null, "episodeId": "ep-1", "currentTime": 120, "isFinished": false},
                {"libraryItemId": "li-1", "episodeId": null, "currentTime": null, "isFinished": null},
                {"libraryItemId": 7, "episodeId": "ep-2", "currentTime": "soon"},
            ],
        }))
        .unwrap();
        assert_eq!(user.media_progress.dropped, 1);
        assert_eq!(
            user.media_progress.entries,
            [
                MediaProgress {
                    library_item_id: None,
                    episode_id: Some("ep-1".into()),
                    current_time: 120.0,
                    is_finished: false,
                },
                MediaProgress {
                    library_item_id: Some("li-1".into()),
                    episode_id: None,
                    current_time: 0.0,
                    is_finished: false,
                },
            ]
        );
    }

    #[test]
    fn a_null_title_decodes_as_empty() {
        let item: LibraryItem = serde_json::from_value(json!({
            "id": "li-1",
            "mediaType": "podcast",
            "media": {
                "metadata": {"title": null, "author": null},
                "episodes": [{"id": "ep-1", "title": null, "description": null,
                              "publishedAt": null, "duration": null, "audioTrack": null}],
            },
        }))
        .unwrap();
        let Media::Podcast(podcast) = item.media else {
            panic!("{item:?}");
        };
        assert_eq!(podcast.metadata.title, "");
        assert_eq!(podcast.episodes[0].title, "");
    }

    /// Audiobookshelf answers 403 for a Library or item the user may not see.
    #[tokio::test]
    async fn a_forbidden_library_or_item_is_not_found() {
        let forbidden = || async { StatusCode::FORBIDDEN };
        let base_url = serve(
            Router::new()
                .route("/api/items/li-1", get(forbidden))
                .route("/api/libraries/lib-1/items", get(forbidden)),
        )
        .await;
        let client = Client::new();
        let key = ApiKey::new("key");
        assert!(matches!(
            client.library_item(&base_url, &key, "li-1").await,
            Err(Error::NotFound(NotFound::Forbidden))
        ));
        assert!(matches!(
            client.library_items(&base_url, &key, "lib-1").await,
            Err(Error::NotFound(NotFound::Forbidden))
        ));
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
                    Err(Error::NotFound(NotFound::InvalidId))
                ),
                "{id:?}"
            );
            assert!(
                matches!(
                    client.library_items("http://127.0.0.1:9", &key, id).await,
                    Err(Error::NotFound(NotFound::InvalidId))
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
            Err(Error::NotFound(NotFound::Missing))
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

    /// A 403 or 404 on the Libraries list comes from something in front of Audiobookshelf:
    /// the Library Server gave no usable answer, it doesn't mean "no such Library".
    #[tokio::test]
    async fn a_forbidden_or_missing_libraries_list_is_unavailable() {
        for status in [StatusCode::FORBIDDEN, StatusCode::NOT_FOUND] {
            let base_url =
                serve(Router::new().route("/api/libraries", get(move || async move { status })))
                    .await;
            assert!(
                matches!(
                    Client::new()
                        .libraries(&base_url, &ApiKey::new("key"))
                        .await,
                    Err(Error::Unavailable(_))
                ),
                "{status}"
            );
        }
    }

    /// A Library in a shape this client doesn't know (another kind, for example) is left out,
    /// not the whole list.
    #[tokio::test]
    async fn a_library_in_an_unknown_shape_is_left_out() {
        let base_url = serve(Router::new().route(
            "/api/libraries",
            get(|| async {
                axum::Json(json!({"libraries": [
                    {"id": "l1", "name": "Books", "mediaType": "book"},
                    {"id": "l2", "name": "Comics", "mediaType": "comic"},
                    {"id": "l3", "name": "Shows", "mediaType": "podcast"},
                ]}))
            }),
        ))
        .await;
        let libraries = Client::new()
            .libraries(&base_url, &ApiKey::new("key"))
            .await
            .unwrap();
        let kinds: Vec<_> = libraries
            .entries
            .iter()
            .map(|library| (library.id.as_str(), library.media_type))
            .collect();
        assert_eq!(kinds, [("l1", MediaType::Book), ("l3", MediaType::Podcast)]);
        assert_eq!(libraries.dropped, 1);
    }

    /// Audiobookshelf sends 0 (or null) for a duration it doesn't know; that is no duration.
    #[test]
    fn a_zero_or_null_duration_is_unknown() {
        let item: LibraryItem = serde_json::from_value(json!({
            "id": "li-1",
            "mediaType": "podcast",
            "media": {
                "metadata": {"title": "Podcast"},
                "episodes": [
                    {"id": "e1", "title": "Zero", "duration": 0,
                     "audioTrack": {"ino": "1", "startOffset": 0, "duration": 0}},
                    {"id": "e2", "title": "Null", "duration": null},
                    {"id": "e3", "title": "Known", "duration": 60.5},
                ],
            },
        }))
        .unwrap();
        let Media::Podcast(podcast) = item.media else {
            panic!("{item:?}");
        };
        let durations: Vec<_> = podcast
            .episodes
            .iter()
            .map(|episode| episode.duration)
            .collect();
        assert_eq!(durations, [None, None, Some(60.5)]);
        assert_eq!(
            podcast.episodes[0].audio_track.as_ref().unwrap().duration,
            None
        );
    }

    /// Only an explicit 0 says a book has no audio; without `numTracks` it is not known.
    #[test]
    fn a_book_without_num_tracks_is_not_taken_for_an_ebook() {
        let book = |media: serde_json::Value| -> Option<usize> {
            let item: LibraryItem =
                serde_json::from_value(json!({"id": "li", "mediaType": "book", "media": media}))
                    .unwrap();
            let Media::Book(book) = item.media else {
                panic!("{item:?}");
            };
            book.num_tracks
        };
        assert_eq!(book(json!({"metadata": {"title": "B"}})), None);
        assert_eq!(
            book(json!({"metadata": {"title": "B"}, "numTracks": 0})),
            Some(0)
        );
        assert_eq!(
            book(json!({"metadata": {"title": "B"}, "numTracks": 3})),
            Some(3)
        );
    }

    /// A Library's contents come in one request without `limit` or `page` (Audiobookshelf
    /// then answers with all of them), and every result counts.
    #[tokio::test]
    async fn a_library_is_read_in_one_request_without_paging() {
        let base_url = serve(Router::new().route(
            "/api/libraries/lib-1/items",
            get(|axum::extract::RawQuery(query): axum::extract::RawQuery| async move {
                let query = query.unwrap_or_default();
                if query.contains("limit") || query.contains("page") {
                    return (StatusCode::BAD_REQUEST, axum::Json(json!({}))).into_response();
                }
                let results: Vec<_> = (0..250)
                    .map(|n| json!({"id": format!("li-{n}"), "mediaType": "book",
                                    "media": {"metadata": {"title": format!("Book {n}")}, "numTracks": 1}}))
                    .collect();
                axum::Json(json!({"results": results, "total": 250})).into_response()
            }),
        ))
        .await;
        let items = Client::new()
            .library_items(&base_url, &ApiKey::new("key"), "lib-1")
            .await
            .unwrap();
        assert_eq!(items.len(), 250);
    }

    /// Audiobookshelf keeps a file's duration null when it could not probe it; that file must
    /// not fail the whole item.
    #[test]
    fn a_track_without_a_duration_decodes() {
        let item: LibraryItem = serde_json::from_value(json!({
            "id": "li-1",
            "mediaType": "book",
            "media": {
                "metadata": {"title": "Book"},
                "duration": 0,
                "numTracks": 1,
                "tracks": [{"ino": "7", "startOffset": null, "duration": null}],
            },
        }))
        .unwrap();
        let Media::Book(book) = item.media else {
            panic!("{item:?}");
        };
        assert_eq!(book.tracks[0].duration, None);
        assert_eq!(book.tracks[0].start_offset, None);
        assert_eq!(book.duration, None);
    }

    /// Chapters of null, or a chapter in a shape this client can't read, must not fail the
    /// whole audiobook.
    #[test]
    fn unreadable_chapters_are_left_out() {
        let book = |chapters: serde_json::Value| -> Known<super::Chapter> {
            let item: LibraryItem = serde_json::from_value(json!({
                "id": "li", "mediaType": "book",
                "media": {"metadata": {"title": "B"}, "numTracks": 1, "chapters": chapters},
            }))
            .unwrap();
            let Media::Book(book) = item.media else {
                panic!("{item:?}");
            };
            book.chapters
        };
        assert_eq!(book(json!(null)), Known::default());
        let chapters = book(json!([
            {"title": "One", "start": 0, "end": 10},
            {"title": "Two", "start": "ten", "end": 20},
        ]));
        let titles: Vec<_> = chapters
            .entries
            .iter()
            .map(|chapter| chapter.title.as_str())
            .collect();
        assert_eq!(titles, ["One"]);
        assert_eq!(chapters.dropped, 1);
    }
}
