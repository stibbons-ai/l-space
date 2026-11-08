//! The Audiobookshelf client: the Bridge's only way to talk to a Library Server.
//!
//! It covers the "who am I" check (`GET /api/me`) that proves an API key (ADR 0011) and also
//! brings the user's Progress, read-only browsing: the user's Libraries, a Library's
//! audiobooks or podcasts, one audiobook or podcast with its chapters, audio files and
//! episodes, the audiobooks and episodes the user is in the middle of, and a podcast Library's
//! newest episodes the user hasn't Finished; marking an audiobook or episode Finished or
//! unfinished for the user; and an audio file's bytes, by byte range, for the Bridge's streams
//! (ADR 0010).
//! Every request authenticates with `Authorization: Bearer <api key>`.
//!
//! Ids a caller passes go into request paths only if they are plain Audiobookshelf ids (ASCII
//! letters, digits, `-` and `_`); any other id names nothing ([`Error::NotFound`]) and is
//! never sent.

use std::fmt;
use std::time::Duration;

use bytes::Bytes;
use futures::stream::{self, BoxStream};
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, HeaderValue, RANGE};
use reqwest::{Method, RequestBuilder, Response, StatusCode, redirect};
use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Deserializer, Serialize};

/// How long one request to a Library Server may take, connecting included, before it counts
/// as unreachable. For an audio file, how long connecting may take, and then the answer and
/// each next part of its body: the body itself may take as long as a speaker takes to read it.
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
/// out: one entry in a shape this client doesn't know must not fail the whole answer. A
/// `null` list is empty; a field of this type that must be in the answer needs
/// `#[serde(deserialize_with = "Known::deserialize")]`, as serde otherwise decodes it missing
/// as empty too.
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
    /// The audio file as Audiobookshelf keeps it: the only one an episode in
    /// [`Client::items_in_progress`] comes with (without `duration` and `audioTrack`).
    #[serde(default)]
    pub audio_file: Option<Track>,
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
    /// The file itself, as Audiobookshelf found it; none if it doesn't say.
    #[serde(default)]
    pub metadata: Option<FileMetadata>,
}

/// What Audiobookshelf found out about an audio file on disk.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FileMetadata {
    /// Its name, with its extension, without its directory.
    #[serde(default, deserialize_with = "null_as_default")]
    pub filename: String,
}

/// An audiobook or episode the user has unfinished Progress in, as
/// [`Client::items_in_progress`] lists it: its library item (only the metadata, a book's
/// `numTracks` and `duration` filled in), with the episode for a podcast's.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InProgress {
    /// The Library holding it.
    pub library_id: String,
    #[serde(flatten)]
    pub library_item: LibraryItem,
    /// The episode the Progress is in, for a podcast.
    pub recent_episode: Option<Episode>,
    /// When the user's Progress in it last changed, in milliseconds since the Unix epoch.
    pub progress_last_update: Option<i64>,
}

/// An episode as [`Client::recent_episodes`] lists it, with its podcast.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentEpisode {
    /// The podcast (its library item).
    pub library_item_id: String,
    #[serde(flatten)]
    pub episode: Episode,
    /// When Audiobookshelf added it, in milliseconds since the Unix epoch.
    pub added_at: Option<i64>,
    pub podcast: EpisodePodcast,
}

/// The podcast a [`RecentEpisode`] belongs to.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EpisodePodcast {
    pub metadata: PodcastMetadata,
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

/// A client for any number of Library Servers. Cloning it is cheap: clones share connections.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    /// For audio files, whose bodies have no time limit in all.
    media: reqwest::Client,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        // reqwest drops `Authorization` on a redirect to another host or port, but would send
        // the key along on a redirect to the same host and port, even over plain http; a
        // request to Audiobookshelf has no reason to move.
        let builder = || reqwest::Client::builder().redirect(redirect::Policy::none());
        let built = "the TLS backend and system configuration load";
        let http = builder().timeout(TIMEOUT).build().expect(built);
        let media = builder()
            .connect_timeout(TIMEOUT)
            .read_timeout(TIMEOUT)
            .build()
            .expect(built);
        Self { http, media }
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
            #[serde(deserialize_with = "Known::deserialize")]
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

    /// Audio file `ino` of the audiobook or podcast `library_item_id`, as the Library Server
    /// serves it (`/api/items/<id>/file/<ino>`): `request` is `GET` or `HEAD`, and `range`
    /// the `Range` header to send, if any. The answer's body streams; a failure while reading
    /// it ends it with an error.
    pub async fn file(
        &self,
        base_url: &str,
        api_key: &ApiKey,
        library_item_id: &str,
        ino: &str,
        request: FileRequest,
        range: Option<&str>,
    ) -> Result<File, Error> {
        let (library_item_id, ino) = (id_segment(library_item_id)?, id_segment(ino)?);
        let method = match request {
            FileRequest::Get => Method::GET,
            FileRequest::Head => Method::HEAD,
        };
        let mut request = self
            .media
            .request(
                method,
                format!("{base_url}/api/items/{library_item_id}/file/{ino}"),
            )
            .header(AUTHORIZATION, authorization(api_key)?);
        if let Some(range) = range {
            let range = HeaderValue::from_str(range)
                .map_err(|_| Error::Unavailable("a Range that is no header value".to_owned()))?;
            request = request.header(RANGE, range);
        }
        let response = request.send().await.map_err(Error::unavailable)?;
        let status = response.status();
        match status {
            StatusCode::UNAUTHORIZED => return Err(Error::Rejected),
            StatusCode::FORBIDDEN => return Err(Error::NotFound(NotFound::Forbidden)),
            StatusCode::NOT_FOUND => return Err(Error::NotFound(NotFound::Missing)),
            StatusCode::OK | StatusCode::PARTIAL_CONTENT | StatusCode::RANGE_NOT_SATISFIABLE => {}
            status => return Err(Error::Unavailable(format!("HTTP {status}"))),
        }
        let header = |name| {
            let value: &HeaderValue = response.headers().get(name)?;
            value.to_str().ok()
        };
        let content_length = header(CONTENT_LENGTH).and_then(|length| length.parse().ok());
        let content_range = header(CONTENT_RANGE).map(str::to_owned);
        let body = stream::unfold(Some(response), async |response| {
            let mut response = response?;
            match response.chunk().await {
                Ok(Some(chunk)) => Some((Ok(chunk), Some(response))),
                Ok(None) => None,
                Err(error) => Some((Err(Error::unavailable(error)), None)),
            }
        });
        Ok(File {
            status: status.as_u16(),
            content_length,
            content_range,
            body: Box::pin(body),
        })
    }

    /// Marks the book `library_item_id`, or the episode `episode_id` of the podcast
    /// `library_item_id`, Finished (`finished`) or unfinished for the user `api_key` acts for:
    /// `PATCH /api/me/progress/<library item>[/<episode>]` with `{"isFinished": finished}`.
    ///
    /// As Audiobookshelf 2.37.2 does it (`User.createUpdateMediaProgressFromPayload`,
    /// `MediaProgress.applyProgressUpdate`): the user gets Progress at 0 seconds if they had
    /// none; marking a Finished one unfinished also sets its position back to 0. An unknown
    /// book or episode is [`Error::NotFound`]; a podcast without an episode is
    /// [`Error::Unavailable`] (HTTP 400), while a book without audio (an ebook alone) is
    /// accepted.
    ///
    /// Callers must check first that the user may see the Item: Audiobookshelf checks neither
    /// that the user may see the library item nor that the episode belongs to it (it finds an
    /// episode by its id alone), and the library item `batch` with the episode `update` reaches
    /// its batch route (`/api/me/progress/batch/update`) instead.
    pub async fn set_finished(
        &self,
        base_url: &str,
        api_key: &ApiKey,
        library_item_id: &str,
        episode_id: Option<&str>,
        finished: bool,
    ) -> Result<(), Error> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Update {
            is_finished: bool,
        }
        let url = progress_url(base_url, library_item_id, episode_id)?;
        let request = self.http.patch(url).json(&Update {
            is_finished: finished,
        });
        self.send(request, api_key, Names::One).await?;
        Ok(())
    }

    /// Sets the Progress of the user `api_key` acts for in the book `library_item_id`, or the
    /// episode `episode_id` of the podcast `library_item_id`, to `current_time` seconds in:
    /// `PATCH /api/me/progress/<library item>[/<episode>]` with `{"currentTime": …}`, plus
    /// `duration` (seconds) and the `progress` fraction it makes when the length is known.
    /// Finished is not sent.
    ///
    /// As Audiobookshelf 2.37.2 does it (`User.createUpdateMediaProgressFromPayload`,
    /// `MediaProgress.applyProgressUpdate`): the user gets Progress if they had none; with a
    /// length, Audiobookshelf itself marks the Progress Finished when under 10 seconds remain,
    /// and unfinished when an earlier Finished one moves away from there. Fails as
    /// [`Client::set_finished`] does. Audiobookshelf doesn't check that the user may see the
    /// Item: a caller that needs it checks that itself.
    pub async fn set_progress(
        &self,
        base_url: &str,
        api_key: &ApiKey,
        library_item_id: &str,
        episode_id: Option<&str>,
        current_time: f64,
        duration: Option<f64>,
    ) -> Result<(), Error> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Update {
            current_time: f64,
            #[serde(skip_serializing_if = "Option::is_none")]
            duration: Option<f64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            progress: Option<f64>,
        }
        let url = progress_url(base_url, library_item_id, episode_id)?;
        let duration = duration.filter(|duration| duration.is_finite() && *duration > 0.0);
        let request = self.http.patch(url).json(&Update {
            current_time,
            duration,
            progress: duration.map(|duration| (current_time / duration).clamp(0.0, 1.0)),
        });
        self.send(request, api_key, Names::One).await?;
        Ok(())
    }

    /// The audiobooks and episodes the user has unfinished Progress in, most recently
    /// listened to first, at most `limit`: `GET /api/me/items-in-progress`. One in a shape
    /// this client doesn't know is left out and counted.
    ///
    /// As Audiobookshelf 2.37.2 answers (`MeController.getAllLibraryItemsInProgress`): one
    /// entry per audiobook or episode whose Progress isn't Finished and is past 0 seconds, or
    /// has ebook progress (so an ebook alone may be listed), ordered by when that Progress last
    /// changed; from every Library, also those the user may not see; "hide from continue
    /// listening" is not honoured.
    pub async fn items_in_progress(
        &self,
        base_url: &str,
        api_key: &ApiKey,
        limit: usize,
    ) -> Result<Known<InProgress>, Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct InProgressItems {
            #[serde(deserialize_with = "Known::deserialize")]
            library_items: Known<InProgress>,
        }
        let path = format!("/api/me/items-in-progress?limit={limit}");
        let items: InProgressItems = self.get(base_url, api_key, &path, Names::Nothing).await?;
        Ok(items.library_items)
    }

    /// The newest episodes in podcast Library `library_id` that the user hasn't Finished, at
    /// most `limit`: `GET /api/libraries/<id>/recent-episodes`. One in a shape this client
    /// doesn't know is left out and counted.
    ///
    /// As Audiobookshelf 2.37.2 answers (`LibraryController.getRecentEpisodes`): by publish
    /// date, newest first; only this user's Progress counts.
    pub async fn recent_episodes(
        &self,
        base_url: &str,
        api_key: &ApiKey,
        library_id: &str,
        limit: usize,
    ) -> Result<Known<RecentEpisode>, Error> {
        #[derive(Deserialize)]
        struct RecentEpisodes {
            #[serde(deserialize_with = "Known::deserialize")]
            episodes: Known<RecentEpisode>,
        }
        let library_id = id_segment(library_id)?;
        let path = format!("/api/libraries/{library_id}/recent-episodes?limit={limit}");
        let episodes: RecentEpisodes = self.get(base_url, api_key, &path, Names::One).await?;
        Ok(episodes.episodes)
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
        let request = self.http.get(format!("{base_url}{path_and_query}"));
        let response = self.send(request, api_key, names).await?;
        response.json().await.map_err(Error::unavailable)
    }

    /// Sends `request`, which `names` a Library or library item or not, with `api_key`; the
    /// answer if it is a success.
    async fn send(
        &self,
        request: RequestBuilder,
        api_key: &ApiKey,
        names: Names,
    ) -> Result<Response, Error> {
        let response = request
            .header(AUTHORIZATION, authorization(api_key)?)
            .send()
            .await
            .map_err(Error::unavailable)?;
        match (response.status(), names) {
            (StatusCode::UNAUTHORIZED, _) => Err(Error::Rejected),
            // Audiobookshelf answers 403 for a Library or item the user may not see.
            (StatusCode::FORBIDDEN, Names::One) => Err(Error::NotFound(NotFound::Forbidden)),
            (StatusCode::NOT_FOUND, Names::One) => Err(Error::NotFound(NotFound::Missing)),
            (status, _) if status.is_success() => Ok(response),
            // Any other answer, for example a server error, a redirect, or a 403/404 on
            // `/api/me` or `/api/libraries`, which every Audiobookshelf answers, so something
            // in front of it did.
            (status, _) => Err(Error::Unavailable(format!("HTTP {status}"))),
        }
    }
}

/// How to ask for an audio file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRequest {
    Get,
    /// The headers alone.
    Head,
}

/// An audio file, or the part of it asked for, as the Library Server answered.
pub struct File {
    /// 200 (the whole file), 206 (the range asked for) or 416 (a range the file doesn't
    /// have).
    pub status: u16,
    /// The body's length, as the Library Server said.
    pub content_length: Option<u64>,
    /// Which bytes of the file the body holds (206), or how long the file is (416).
    pub content_range: Option<String>,
    /// The bytes; empty for `HEAD`.
    pub body: BoxStream<'static, Result<Bytes, Error>>,
}

impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("File")
            .field("status", &self.status)
            .field("content_length", &self.content_length)
            .field("content_range", &self.content_range)
            .finish_non_exhaustive()
    }
}

/// The `Authorization` header for `api_key`, marked sensitive. A key that cannot be sent as a
/// header value cannot be a valid key.
fn authorization(api_key: &ApiKey) -> Result<HeaderValue, Error> {
    let mut authorization = HeaderValue::try_from(format!("Bearer {}", api_key.expose()))
        .map_err(|_| Error::Rejected)?;
    authorization.set_sensitive(true);
    Ok(authorization)
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

/// The URL of the user's Progress in the book `library_item_id`, or in the episode
/// `episode_id` of the podcast `library_item_id`: `/api/me/progress/<library item>[/<episode>]`.
fn progress_url(
    base_url: &str,
    library_item_id: &str,
    episode_id: Option<&str>,
) -> Result<String, Error> {
    let library_item_id = id_segment(library_item_id)?;
    Ok(match episode_id {
        Some(episode_id) => format!(
            "{base_url}/api/me/progress/{library_item_id}/{}",
            id_segment(episode_id)?
        ),
        None => format!("{base_url}/api/me/progress/{library_item_id}"),
    })
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
            assert!(
                matches!(
                    client
                        .recent_episodes("http://127.0.0.1:9", &key, id, 20)
                        .await,
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

    /// Every request the stand-in got: method, path, `Authorization` and JSON body (null if
    /// none).
    type Requests =
        std::sync::Arc<std::sync::Mutex<Vec<(String, String, String, serde_json::Value)>>>;

    /// A stand-in that records every request and answers each with `answer`.
    async fn recording(
        answer: impl IntoResponse + Clone + Send + Sync + 'static,
    ) -> (String, Requests) {
        let requests = Requests::default();
        let recorded = requests.clone();
        let base_url = serve(Router::new().fallback(
            move |method: axum::http::Method,
                  uri: axum::http::Uri,
                  headers: HeaderMap,
                  body: String| async move {
                let authorization = headers
                    .get("authorization")
                    .map(|value| value.to_str().unwrap().to_owned())
                    .unwrap_or_default();
                recorded.lock().unwrap().push((
                    method.to_string(),
                    uri.to_string(),
                    authorization,
                    serde_json::from_str(&body).unwrap_or_default(),
                ));
                answer
            },
        ))
        .await;
        (base_url, requests)
    }

    #[tokio::test]
    async fn marking_finished_patches_the_users_progress_in_the_item_or_episode() {
        let (base_url, requests) = recording(StatusCode::OK).await;
        let client = Client::new();
        let key = ApiKey::new("s3cret");
        client
            .set_finished(&base_url, &key, "li-1", None, true)
            .await
            .unwrap();
        client
            .set_finished(&base_url, &key, "li-2", Some("ep-1"), false)
            .await
            .unwrap();

        assert_eq!(
            *requests.lock().unwrap(),
            [
                (
                    "PATCH".to_owned(),
                    "/api/me/progress/li-1".to_owned(),
                    "Bearer s3cret".to_owned(),
                    json!({"isFinished": true}),
                ),
                (
                    "PATCH".to_owned(),
                    "/api/me/progress/li-2/ep-1".to_owned(),
                    "Bearer s3cret".to_owned(),
                    json!({"isFinished": false}),
                ),
            ]
        );
    }

    #[tokio::test]
    async fn marking_finished_fails_like_fetching_an_item() {
        let cases = [
            (StatusCode::UNAUTHORIZED, "Rejected"),
            (StatusCode::FORBIDDEN, "NotFound(Forbidden)"),
            (StatusCode::NOT_FOUND, "NotFound(Missing)"),
            // Audiobookshelf's answer for a library item that is no audiobook.
            (
                StatusCode::BAD_REQUEST,
                "Unavailable(\"HTTP 400 Bad Request\")",
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unavailable(\"HTTP 500 Internal Server Error\")",
            ),
        ];
        for (status, expected) in cases {
            let (base_url, _) = recording(status).await;
            let error = Client::new()
                .set_finished(&base_url, &ApiKey::new("key"), "li-1", Some("ep-1"), true)
                .await
                .unwrap_err();
            assert_eq!(format!("{error:?}"), expected, "{status}");
        }
    }

    #[tokio::test]
    async fn marking_finished_with_an_id_that_is_not_an_audiobookshelf_id_sends_nothing() {
        let (base_url, requests) = recording(StatusCode::OK).await;
        let client = Client::new();
        let key = ApiKey::new("key");
        for (item, episode) in [
            ("../me", None),
            ("", None),
            ("li-1", Some("../../li-2")),
            ("li-1", Some("")),
            ("li-1", Some("ep?x")),
        ] {
            assert!(
                matches!(
                    client
                        .set_finished(&base_url, &key, item, episode, true)
                        .await,
                    Err(Error::NotFound(NotFound::InvalidId))
                ),
                "{item:?} {episode:?}"
            );
        }
        assert!(requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn setting_progress_patches_the_current_time_with_the_length_when_known() {
        let (base_url, requests) = recording(StatusCode::OK).await;
        let client = Client::new();
        let key = ApiKey::new("s3cret");
        client
            .set_progress(&base_url, &key, "li-1", None, 150.0, Some(600.0))
            .await
            .unwrap();
        client
            .set_progress(&base_url, &key, "li-2", Some("ep-1"), 42.0, None)
            .await
            .unwrap();
        // A length Audiobookshelf would take as unknown is left out.
        client
            .set_progress(&base_url, &key, "li-2", Some("ep-1"), 0.0, Some(0.0))
            .await
            .unwrap();

        assert_eq!(
            *requests.lock().unwrap(),
            [
                (
                    "PATCH".to_owned(),
                    "/api/me/progress/li-1".to_owned(),
                    "Bearer s3cret".to_owned(),
                    json!({"currentTime": 150.0, "duration": 600.0, "progress": 0.25}),
                ),
                (
                    "PATCH".to_owned(),
                    "/api/me/progress/li-2/ep-1".to_owned(),
                    "Bearer s3cret".to_owned(),
                    json!({"currentTime": 42.0}),
                ),
                (
                    "PATCH".to_owned(),
                    "/api/me/progress/li-2/ep-1".to_owned(),
                    "Bearer s3cret".to_owned(),
                    json!({"currentTime": 0.0}),
                ),
            ]
        );
    }

    #[tokio::test]
    async fn setting_progress_fails_like_marking_finished() {
        let cases = [
            (StatusCode::UNAUTHORIZED, "Rejected"),
            (StatusCode::FORBIDDEN, "NotFound(Forbidden)"),
            (StatusCode::NOT_FOUND, "NotFound(Missing)"),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unavailable(\"HTTP 500 Internal Server Error\")",
            ),
        ];
        for (status, expected) in cases {
            let (base_url, _) = recording(status).await;
            let error = Client::new()
                .set_progress(&base_url, &ApiKey::new("key"), "li-1", None, 1.0, None)
                .await
                .unwrap_err();
            assert_eq!(format!("{error:?}"), expected, "{status}");
        }

        let (base_url, requests) = recording(StatusCode::OK).await;
        for (item, episode) in [("../me", None), ("li-1", Some("../../li-2"))] {
            assert!(
                matches!(
                    Client::new()
                        .set_progress(&base_url, &ApiKey::new("key"), item, episode, 1.0, None)
                        .await,
                    Err(Error::NotFound(NotFound::InvalidId))
                ),
                "{item:?} {episode:?}"
            );
        }
        assert!(requests.lock().unwrap().is_empty());
    }

    /// Shaped like what Audiobookshelf 2.37.2 answers for `/api/me/items-in-progress`, trimmed:
    /// an audiobook and an episode with its podcast; plus a made-up entry of a kind this client
    /// doesn't know.
    fn items_in_progress_answer() -> serde_json::Value {
        json!({"libraryItems": [
            {"id": "li-book", "libraryId": "lib-books", "mediaType": "book",
             "media": {"metadata": {"title": "The Long Book", "authorName": "Pratchett"},
                       "numTracks": 3, "duration": 4320},
             "progressLastUpdate": 1_793_967_063_333_i64},
            {"id": "li-pod", "libraryId": "lib-pods", "mediaType": "podcast",
             "media": {"metadata": {"title": "The Clacks Hour", "author": "Radio"}},
             "recentEpisode": {"id": "ep-2", "title": "Episode 2", "publishedAt": 1_771_113_600_000_i64,
                               "description": null,
                               "audioFile": {"ino": "7", "duration": 900}},
             "progressLastUpdate": 1_793_967_063_320_i64},
            {"id": "li-comic", "libraryId": "lib-x", "mediaType": "comic", "media": {}},
        ]})
    }

    #[tokio::test]
    async fn items_in_progress_are_asked_for_with_the_key_and_a_limit() {
        let (base_url, requests) =
            recording((StatusCode::OK, axum::Json(items_in_progress_answer()))).await;
        let in_progress = Client::new()
            .items_in_progress(&base_url, &ApiKey::new("s3cret"), 40)
            .await
            .unwrap();

        assert_eq!(
            *requests.lock().unwrap(),
            [(
                "GET".to_owned(),
                "/api/me/items-in-progress?limit=40".to_owned(),
                "Bearer s3cret".to_owned(),
                serde_json::Value::Null,
            )]
        );
        assert_eq!(in_progress.dropped, 1);
        let [book, episode] = &in_progress.entries[..] else {
            panic!("{in_progress:?}");
        };
        assert_eq!(
            (book.library_id.as_str(), book.library_item.id.as_str()),
            ("lib-books", "li-book")
        );
        assert_eq!(book.progress_last_update, Some(1_793_967_063_333));
        assert_eq!(book.recent_episode, None);
        let Media::Book(media) = &book.library_item.media else {
            panic!("{book:?}");
        };
        assert_eq!(
            (
                media.metadata.author_name.as_deref(),
                media.duration,
                media.num_tracks
            ),
            (Some("Pratchett"), Some(4320.0), Some(3))
        );
        let Media::Podcast(podcast) = &episode.library_item.media else {
            panic!("{episode:?}");
        };
        assert_eq!(podcast.metadata.title, "The Clacks Hour");
        let recent = episode.recent_episode.as_ref().expect("its episode");
        assert_eq!(
            (
                recent.id.as_str(),
                recent.title.as_str(),
                recent.published_at,
                recent.duration,
                recent.audio_file.as_ref().and_then(|file| file.duration),
            ),
            (
                "ep-2",
                "Episode 2",
                Some(1_771_113_600_000),
                None,
                Some(900.0)
            )
        );
    }

    #[tokio::test]
    async fn recent_episodes_are_asked_for_by_library_with_the_key_and_a_limit() {
        let answer = json!({"episodes": [
            {"libraryItemId": "li-pod", "id": "ep-3", "title": "Episode 3",
             "publishedAt": null, "addedAt": 1_793_967_063_014_i64, "duration": 3960,
             "podcast": {"metadata": {"title": "The Clacks Hour", "author": "Radio"}}},
            {"libraryItemId": "li-pod", "id": "ep-x", "title": "No podcast"},
        ]});
        let (base_url, requests) = recording((StatusCode::OK, axum::Json(answer))).await;
        let episodes = Client::new()
            .recent_episodes(&base_url, &ApiKey::new("s3cret"), "lib-pods", 20)
            .await
            .unwrap();

        assert_eq!(
            *requests.lock().unwrap(),
            [(
                "GET".to_owned(),
                "/api/libraries/lib-pods/recent-episodes?limit=20".to_owned(),
                "Bearer s3cret".to_owned(),
                serde_json::Value::Null,
            )]
        );
        assert_eq!(episodes.dropped, 1);
        let [episode] = &episodes.entries[..] else {
            panic!("{episodes:?}");
        };
        assert_eq!(episode.library_item_id, "li-pod");
        assert_eq!(
            (episode.episode.id.as_str(), episode.episode.published_at),
            ("ep-3", None)
        );
        assert_eq!(episode.added_at, Some(1_793_967_063_014));
        assert_eq!(episode.episode.duration, Some(3960.0));
        assert_eq!(episode.podcast.metadata.title, "The Clacks Hour");
    }

    /// Every Audiobookshelf answers `/api/me/items-in-progress`, so a 403 or 404 there comes
    /// from something in front of it; on a Library's newest episodes it means the user may not
    /// see that Library, or it is gone.
    #[tokio::test]
    async fn home_calls_fail_like_the_calls_they_resemble() {
        let cases = [
            (StatusCode::UNAUTHORIZED, "Rejected", "Rejected"),
            (
                StatusCode::FORBIDDEN,
                "Unavailable(\"HTTP 403 Forbidden\")",
                "NotFound(Forbidden)",
            ),
            (
                StatusCode::NOT_FOUND,
                "Unavailable(\"HTTP 404 Not Found\")",
                "NotFound(Missing)",
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unavailable(\"HTTP 500 Internal Server Error\")",
                "Unavailable(\"HTTP 500 Internal Server Error\")",
            ),
        ];
        let key = ApiKey::new("key");
        for (status, in_progress, recent) in cases {
            let (base_url, _) = recording(status).await;
            let client = Client::new();
            let error = client
                .items_in_progress(&base_url, &key, 40)
                .await
                .unwrap_err();
            assert_eq!(format!("{error:?}"), in_progress, "{status}");
            let error = client
                .recent_episodes(&base_url, &key, "lib-1", 20)
                .await
                .unwrap_err();
            assert_eq!(format!("{error:?}"), recent, "{status}");
        }
    }

    /// An answer without the list a call asks for is no answer from Audiobookshelf, not an
    /// empty list.
    #[tokio::test]
    async fn an_answer_without_its_list_fails() {
        let (base_url, _) = recording((StatusCode::OK, axum::Json(json!({})))).await;
        let (client, key) = (Client::new(), ApiKey::new("key"));
        assert!(matches!(
            client.libraries(&base_url, &key).await,
            Err(Error::Unavailable(_))
        ));
        assert!(matches!(
            client.items_in_progress(&base_url, &key, 40).await,
            Err(Error::Unavailable(_))
        ));
        assert!(matches!(
            client.recent_episodes(&base_url, &key, "lib-1", 20).await,
            Err(Error::Unavailable(_))
        ));
    }
}
