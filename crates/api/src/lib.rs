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
//!
//! A session browses its Library Server as its user: [`home`] lists what the user is in the
//! middle of and the newest episodes they haven't Finished, [`libraries`] the Libraries,
//! [`library_entries`] a Library's audiobooks or podcasts, [`podcast`] a podcast's episodes
//! filtered by the user's Progress, and [`item`] one Item with that Progress, its chapters and
//! its audio files; [`mark_finished`] marks an Item Finished or unfinished for the user. Ids
//! from the App name things on the Library Server; one that names nothing the user can see is
//! [`ApiError::NotFound`].
//!
//! A session also sees the household's Sonos Groups with what each plays, [`groups`], starts
//! an Item on a Group, [`start_handoff`], and sees every active Handoff, [`handoffs`]. Any
//! logged-in household member may start on any Group and sees every Handoff. Any of them may
//! also control any Group's Handoff, whoever started it: [`pause_group`], [`resume_group`],
//! [`skip_group`]; and any Group's volume, [`set_group_volume`]. Pausing and skipping write
//! the new position as Progress for the user who started the Handoff, with their key.

use std::cmp::Reverse;
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

/// The Audiobookshelf user a session acts for. It also names who started a Handoff
/// ([`Handoff::started_by`]), which every logged-in household member sees: whatever is added
/// here goes out to all of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentUser {
    /// The normalized base URL of the user's Library Server.
    pub library_server: String,
    /// Audiobookshelf's id for the user.
    pub user_id: String,
    pub username: String,
}

/// A Library on the session's Library Server: audiobooks or podcasts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    /// Audiobookshelf's id for the Library.
    pub id: String,
    pub name: String,
    pub kind: LibraryKind,
}

/// What a Library holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryKind {
    Audiobooks,
    Podcasts,
}

/// An audiobook or a podcast, as a Library lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryEntry {
    /// Audiobookshelf's id for the audiobook or podcast (its library item).
    pub id: String,
    pub title: String,
    pub author: Option<String>,
}

/// Names an Item: an audiobook, or one episode of a podcast.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemId {
    /// The audiobook, or the podcast the episode belongs to.
    pub library_item: String,
    /// The episode, for a podcast's.
    pub episode: Option<String>,
}

/// An Item with its details, the session user's Progress and its audio files. Times are
/// seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub title: String,
    /// An audiobook's author.
    pub author: Option<String>,
    /// An episode's podcast, by title.
    pub podcast: Option<String>,
    pub description: Option<String>,
    /// Unknown if the Library Server doesn't know it, for an audiobook or an episode alike.
    pub duration: Option<f64>,
    /// When an episode was published, in milliseconds since the Unix epoch.
    pub published_at: Option<i64>,
    /// The session user's Progress; none if the Library Server holds no Progress of theirs
    /// for the Item.
    pub progress: Option<Progress>,
    /// An audiobook's chapters, in the order the Library Server keeps them; an episode has
    /// none.
    pub chapters: Vec<Chapter>,
    /// The audio files in playing order.
    pub audio_files: Vec<AudioFile>,
}

/// How far a user is into an Item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    /// Seconds into the Item.
    pub current_time: f64,
    pub finished: bool,
}

/// A chapter of an audiobook; seconds into the whole audiobook.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chapter {
    pub title: String,
    pub start: f64,
    pub end: f64,
}

/// One audio file of an Item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioFile {
    /// Audiobookshelf's id for the file within its audiobook or podcast.
    pub id: String,
    /// Seconds into the whole Item where this file starts; unknown once a file before it has
    /// an unknown duration, or when the Library Server doesn't say.
    pub start_offset: Option<f64>,
    /// Unknown if the Library Server could not tell.
    pub duration: Option<f64>,
}

/// A podcast with those of its episodes a filter picked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Podcast {
    /// Audiobookshelf's id for the podcast (its library item).
    pub id: String,
    pub title: String,
    pub author: Option<String>,
    /// The filter that picked the episodes: the one asked for, or the one the Bridge applied
    /// instead of a filter it doesn't know.
    pub filter: EpisodeFilter,
    /// Newest first; episodes without a publish date last.
    pub episodes: Vec<Episode>,
}

/// An episode as its podcast lists it. Times are seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Episode {
    /// Audiobookshelf's id for the episode.
    pub id: String,
    pub title: String,
    /// Unknown if the Library Server doesn't know it.
    pub duration: Option<f64>,
    /// Milliseconds since the Unix epoch.
    pub published_at: Option<i64>,
    /// The session user's Progress; none if the Library Server holds no Progress of theirs
    /// for the episode.
    pub progress: Option<Progress>,
}

impl Episode {
    /// The episode `item` is, as its podcast lists it; none if `item` is an audiobook.
    pub fn of(item: &Item) -> Option<Self> {
        Some(Self {
            id: item.id.episode.clone()?,
            title: item.title.clone(),
            duration: item.duration,
            published_at: item.published_at,
            progress: item.progress.clone(),
        })
    }

    /// Its place in its podcast's list, the smallest first: newest first, episodes without a
    /// publish date last.
    pub fn order(&self) -> Reverse<Option<i64>> {
        Reverse(self.published_at)
    }
}

/// How many Items each of Home's lists has at most.
pub const HOME_ENTRIES: usize = 20;

/// What Home lists for the session user, each list at most [`HOME_ENTRIES`] Items, across
/// every Library they can see on the session's Library Server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Home {
    /// Audiobooks and episodes the user has Progress in past the start and hasn't Finished,
    /// the one whose Progress changed last first.
    pub continue_listening: Vec<HomeEntry>,
    /// The newest episodes the user hasn't Finished, of every podcast they can see: newest
    /// publish date first (the date the Library Server added an episode without one). Each
    /// podcast Library contributes its [`HOME_ENTRIES`] newest unfinished by publish date, so
    /// an episode without one shows only when its Library has fewer than [`HOME_ENTRIES`]
    /// unfinished episodes with one.
    pub latest_episodes: Vec<HomeEntry>,
}

/// An Item as Home lists it. Times are seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HomeEntry {
    /// Opens the Item's screen.
    pub id: ItemId,
    pub title: String,
    /// An audiobook's author.
    pub author: Option<String>,
    /// An episode's podcast, by title.
    pub podcast: Option<String>,
    /// Unknown if the Library Server doesn't know it.
    pub duration: Option<f64>,
    /// When an episode was published, in milliseconds since the Unix epoch.
    pub published_at: Option<i64>,
    /// The session user's Progress; none if the Library Server holds no Progress of theirs
    /// for the Item.
    pub progress: Option<Progress>,
}

/// Which of a podcast's episodes to list, by the session user's Progress. A filter this
/// Bridge doesn't know (from a newer App) counts as [`EpisodeFilter::Unfinished`], as does
/// none at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeFilter {
    Finished,
    All,
    /// Not Finished, started or not. Last, as serde wants its catch-all.
    #[default]
    #[serde(other)]
    Unfinished,
}

impl EpisodeFilter {
    /// Every filter, in the order the App offers them.
    pub const CHOICES: [EpisodeFilter; 3] = [Self::Unfinished, Self::Finished, Self::All];

    /// Whether an episode with `progress` passes.
    pub fn passes(self, progress: Option<&Progress>) -> bool {
        let finished = progress.is_some_and(|progress| progress.finished);
        match self {
            Self::Unfinished => !finished,
            Self::Finished => finished,
            Self::All => true,
        }
    }
}

/// A Sonos Group exactly as set up in the Sonos app (the Bridge never changes grouping), with
/// what it plays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    /// Its coordinator's UUID, such as `RINCON_48A6B8000001400`; always the coordinator's, also
    /// when the coordinator is not a member (it is invisible in the Sonos app).
    pub id: String,
    /// The name set in the Sonos app for its coordinator (bonded speakers share it).
    pub name: String,
    /// The players the Sonos app shows in the Group, its coordinator first when it is one of
    /// them; never empty. A single speaker is a Group of one. A bonded surround or sub, the
    /// second speaker of a stereo pair and a Boost are not members.
    pub members: Vec<GroupMember>,
    /// What the Group's playback is doing; none if its coordinator was not found by the
    /// Bridge's searches, didn't answer, or answered too late.
    pub state: Option<TransportState>,
    /// The track it has loaded, if any and if its coordinator said; none whenever `state` is
    /// none.
    pub now_playing: Option<NowPlaying>,
    /// The Group's volume, 0 to [`MAX_VOLUME`]; none if its coordinator didn't tell it in
    /// time, refused to, or told something that isn't such a number (and from a Bridge that
    /// doesn't tell it).
    #[serde(default)]
    pub volume: Option<u8>,
    /// The active Handoff on the Group, whoever started it; none without one (and from a
    /// Bridge that doesn't tell). Its controls act only while the Group plays its stream.
    #[serde(default)]
    pub handoff: Option<Handoff>,
}

/// The highest volume a Group takes; [`set_group_volume`] sets this for anything louder.
pub const MAX_VOLUME: u8 = 100;

/// A Sonos player in a Group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMember {
    /// The player's UUID.
    pub id: String,
    /// The name set in the Sonos app for it.
    pub name: String,
}

/// What a Group's playback is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportState {
    Playing,
    Paused,
    Stopped,
    /// Between states, such as while starting a track.
    Transitioning,
    /// The coordinator's answer had no state, or one the Bridge doesn't know; or (on the App's
    /// side, when decoding) a state from a newer Bridge. Last, as serde wants its catch-all.
    #[serde(other)]
    Unknown,
}

/// The track a Group has loaded. Times are seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NowPlaying {
    /// From the track's metadata, if it has one.
    pub title: Option<String>,
    pub creator: Option<String>,
    /// How far into the track playback is.
    pub position: Option<f64>,
    /// The track's length; none when unknown, as for a stream.
    pub duration: Option<f64>,
}

/// A Handoff: an Item playing on a Group from the Bridge, which keeps it going without the
/// App.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    /// The Group's id, as [`Group::id`].
    pub group: String,
    pub item: ItemId,
    /// Who started it: their Library Server, Audiobookshelf user id, and username then.
    pub started_by: CurrentUser,
    /// When it started, in milliseconds since the Unix epoch.
    pub started_at: i64,
}

/// What a Group plays after a control: what the App shows of it without listing the Groups
/// again. Times are seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Playback {
    pub state: TransportState,
    /// How far into the Item the Group is; none if its coordinator didn't say.
    pub position: Option<f64>,
    /// The Item's length: as the Group tells it, else (pausing or skipping) as the Library
    /// Server tells it; none if unknown.
    pub duration: Option<f64>,
}

/// How far [`skip_group`] moves a Group's position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Skip {
    /// 15 seconds back, but not before the start.
    Back15,
    /// 30 seconds forward, but not past the end.
    Forward30,
}

impl Skip {
    /// The seconds it moves by: negative back, positive forward.
    pub fn seconds(self) -> i32 {
        match self {
            Self::Back15 => -15,
            Self::Forward30 => 30,
        }
    }
}

/// An error code the App translates for the user.
///
/// On the wire this is the `data` member of the error body, for example
/// `{"code":"internal"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ApiError {
    /// The App could not reach the Bridge. Produced only on the App's side, by the client half
    /// of a server function; the Bridge never sends it.
    #[error("bridge_unreachable")]
    BridgeUnreachable,
    /// Something answered at the Bridge's address, but not as a Bridge: an HTTP error without an
    /// App API error code (502, 503 and 504 count as [`ApiError::BridgeUnreachable`] instead), a
    /// redirect (server function calls follow none), or a body the App cannot decode. Produced
    /// only on the App's side; the Bridge never sends it.
    #[error("not_a_bridge")]
    NotABridge,
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
    /// ended (also because the Bridge can no longer decrypt its stored API key, after the
    /// operator changed the encryption key), or one whose Library Server is not configured (any
    /// more). An ended session needs a new login; one whose Library Server is unconfigured may
    /// work again once the operator re-adds it. The App tells the two apart by whether
    /// [`library_servers`] still lists the session's Library Server. [`logout`] doesn't check
    /// the configuration: it ends the session either way.
    #[error("session_invalid")]
    SessionInvalid,
    /// The session's Library Server has no Library or Item with this id that its user can
    /// see.
    #[error("not_found")]
    NotFound,
    /// The Bridge knows Sonos players, but none of them told it the Group topology in time.
    #[error("sonos_unreachable")]
    SonosUnreachable,
    /// The Bridge knows no Sonos player, and can't tell whether there are any: its last search
    /// could not be sent (socket not bound, or datagram not sent), its searching stopped, or
    /// its first search didn't end in time.
    #[error("sonos_search_failed")]
    SonosSearchFailed,
    /// The household has no Group with this id (any more): it was regrouped, or its
    /// coordinator unplugged.
    #[error("group_not_found")]
    GroupNotFound,
    /// The Group did not do what was asked: its coordinator didn't answer in time, or
    /// refused, or it was busy with another start or control on it and wasn't asked.
    #[error("group_failed")]
    GroupFailed,
    /// The Item can't be played on a Group by this Bridge yet: it has more than one audio
    /// file, none, or one in a format Sonos doesn't play.
    #[error("item_not_playable")]
    ItemNotPlayable,
    /// The Group has no active Handoff to control, or plays something other than its
    /// Handoff's stream (switched in the Sonos app, for example: what is a Takeover once
    /// detected, #20, so the Handoff isn't ended yet). No control was sent to the Group and no
    /// Progress written; a Handoff it still has goes on, except after a forward skip that
    /// needed the Item's length: if the Library Server rejected the starter's key there, the
    /// key was revoked, which ended the Handoff (see [`skip_group`]).
    #[error("no_handoff")]
    NoHandoff,
    /// A forward skip was refused because neither the Group nor the Library Server knows the
    /// Item's length, so it has no end to stop at. Nothing was sent to the Group and no
    /// Progress written.
    #[error("length_unknown")]
    LengthUnknown,
    /// A code this build of the App does not know yet, from a newer Bridge. Produced only on
    /// the App's side, when decoding; the Bridge never sends it.
    #[serde(other)]
    #[error("unknown")]
    Unknown,
}

/// How the client half of a server function reports a call that got no App API answer.
impl From<ServerFnError> for ApiError {
    fn from(error: ServerFnError) -> Self {
        // These errors carry the response's body, not the request, so no session token or API
        // key shows in the log.
        tracing::debug!(%error, "server function call failed");
        match error {
            ServerFnError::Request(_) | ServerFnError::StreamError(_) => Self::BridgeUnreachable,
            // A reverse proxy in front of the Bridge that can't reach it answers like this.
            ServerFnError::ServerError {
                code: 502..=504, ..
            } => Self::BridgeUnreachable,
            ServerFnError::ServerError { .. } | ServerFnError::Deserialization(_) => {
                Self::NotABridge
            }
            _ => Self::Internal,
        }
    }
}

impl AsStatusCode for ApiError {
    fn as_status_code(&self) -> StatusCode {
        match self {
            Self::ApiKeyRejected | Self::SessionInvalid => StatusCode::UNAUTHORIZED,
            Self::LibraryServerNotConfigured | Self::NotFound | Self::GroupNotFound => {
                StatusCode::NOT_FOUND
            }
            Self::NoHandoff | Self::LengthUnknown => StatusCode::CONFLICT,
            Self::ItemNotPlayable => StatusCode::UNPROCESSABLE_ENTITY,
            Self::BridgeUnreachable
            | Self::NotABridge
            | Self::LibraryServerUnreachable
            | Self::SonosUnreachable
            | Self::SonosSearchFailed
            | Self::GroupFailed => StatusCode::BAD_GATEWAY,
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

/// Every Library of the session's Library Server that its user can see, audiobooks and
/// podcasts, in the Library Server's order.
#[get(
    "/api/libraries",
    headers: dioxus_fullstack::HeaderMap,
    bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>
)]
pub async fn libraries() -> Result<Vec<Library>, ApiError> {
    bridge.libraries(server::session_token(&headers)?).await
}

/// Every audiobook or podcast in Library `library`, by title.
#[get(
    "/api/library-entries?library",
    headers: dioxus_fullstack::HeaderMap,
    bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>
)]
pub async fn library_entries(library: String) -> Result<Vec<LibraryEntry>, ApiError> {
    bridge
        .library_entries(server::session_token(&headers)?, library)
        .await
}

/// The Item `library_item` (an audiobook), or its episode `episode` (a podcast's), with the
/// session user's Progress.
#[get(
    "/api/item?library_item&episode",
    headers: dioxus_fullstack::HeaderMap,
    bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>
)]
pub async fn item(library_item: String, episode: Option<String>) -> Result<Item, ApiError> {
    let id = ItemId {
        library_item,
        episode,
    };
    bridge.item(server::session_token(&headers)?, id).await
}

/// Marks the Item `item` Finished (`finished`) or unfinished for the session user on the
/// session's Library Server, which keeps Progress; answers with the Item as it is then. The
/// Library Server gives an Item without the user's Progress some at 0 seconds, and sets the
/// position of a Finished one marked unfinished back to 0. An Item the user can't see is
/// [`ApiError::NotFound`], and nothing is written. The write may still have happened when the
/// Library Server took it but reading it back failed: `library_server_unreachable`, or
/// `api_key_rejected` (the read-back was rejected, or the key now acts for another user, and
/// the key was revoked) or `internal` (revoking it failed); so may a mark whose answer was lost.
#[post(
    "/api/item/finished",
    headers: dioxus_fullstack::HeaderMap,
    bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>
)]
pub async fn mark_finished(item: ItemId, finished: bool) -> Result<Item, ApiError> {
    bridge
        .mark_finished(server::session_token(&headers)?, item, finished)
        .await
}

/// The podcast `id` with the episodes `filter` picks by the session user's Progress (by
/// default, and for a filter this Bridge doesn't know, the unfinished ones).
#[get(
    "/api/podcast?id&filter",
    headers: dioxus_fullstack::HeaderMap,
    bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>
)]
pub async fn podcast(id: String, filter: Option<EpisodeFilter>) -> Result<Podcast, ApiError> {
    bridge
        .podcast(
            server::session_token(&headers)?,
            id,
            filter.unwrap_or_default(),
        )
        .await
}

/// Home for the session user: what they are in the middle of, and the newest episodes they
/// haven't Finished.
#[get(
    "/api/home",
    headers: dioxus_fullstack::HeaderMap,
    bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>
)]
pub async fn home() -> Result<Home, ApiError> {
    bridge.home(server::session_token(&headers)?).await
}

/// Every Sonos Group in the Bridge's household, exactly as grouped in the Sonos app, with
/// what each plays, its volume and its active Handoff; by name. None when the Bridge has found
/// no Sonos player.
#[get(
    "/api/groups",
    headers: dioxus_fullstack::HeaderMap,
    bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>
)]
pub async fn groups() -> Result<Vec<Group>, ApiError> {
    bridge.groups(server::session_token(&headers)?).await
}

/// Starts the Item `item` on Group `group` from its beginning, replacing what the Group
/// played and the Handoff it had, if any. The Bridge streams the Item to the Group, which
/// keeps playing without the App. Only an Item with exactly one audio file plays so far. Once
/// the Item is found playable, the Group's Handoff is replaced before the Group is told to
/// play: if it then doesn't start (a Sonos error), the Group is left with no Handoff.
#[post(
    "/api/handoffs/start",
    headers: dioxus_fullstack::HeaderMap,
    bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>
)]
pub async fn start_handoff(item: ItemId, group: String) -> Result<Handoff, ApiError> {
    bridge
        .start_handoff(server::session_token(&headers)?, item, group)
        .await
}

/// Every active Handoff in the household, whoever started it; the earliest started first.
#[get(
    "/api/handoffs",
    headers: dioxus_fullstack::HeaderMap,
    bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>
)]
pub async fn handoffs() -> Result<Vec<Handoff>, ApiError> {
    bridge.handoffs(server::session_token(&headers)?).await
}

/// Pauses Group `group`'s Handoff, whoever started it, and writes where it paused as Progress
/// for the user who started it, with their key (not the caller's), once their Library Server
/// confirms the key still acts for them. Where it paused is read from the Group right after
/// the pause; if that read fails, is late or tells no position, where it was just before
/// pausing. Answers with the Group's playback then; a Group already paused is left as it is.
/// Only while the Group plays its Handoff's stream: otherwise [`ApiError::NoHandoff`], and no
/// control is sent to the Group. A Group that tells no position either time pauses, but no
/// Progress is written. Writing the Progress may fail (the Library Server unreachable or too
/// slow, say): the Group paused all the same, and the call succeeds; if the Library Server
/// rejected the starter's key, or it now acts for another user, it is revoked, which ends the
/// Handoff.
#[post(
    "/api/groups/pause",
    headers: dioxus_fullstack::HeaderMap,
    bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>
)]
pub async fn pause_group(group: String) -> Result<Playback, ApiError> {
    bridge
        .pause_group(server::session_token(&headers)?, group)
        .await
}

/// Has Group `group` play its Handoff on from where it is, whoever started it; writes no
/// Progress. Answers with the Group's playback then; a Group already playing is left as it
/// is. Only while the Group plays its Handoff's stream, as for [`pause_group`].
#[post(
    "/api/groups/resume",
    headers: dioxus_fullstack::HeaderMap,
    bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>
)]
pub async fn resume_group(group: String) -> Result<Playback, ApiError> {
    bridge
        .resume_group(server::session_token(&headers)?, group)
        .await
}

/// Moves Group `group`'s Handoff, whoever started it, 15 seconds back or 30 forward from
/// where it is now, kept between the Item's start and its end (as the Group tells its length,
/// else the Library Server); playing or paused as it was. Writes the new position as Progress
/// as [`pause_group`] does, and answers likewise. A forward skip on a Group that tells no
/// length first asks the Library Server for it: if the Library Server doesn't tell it
/// (unreachable, too slow, or the Item is gone there), the skip is refused with
/// [`ApiError::LibraryServerUnreachable`]; if it knows no length either, with
/// [`ApiError::LengthUnknown`]. If the Library Server rejects the starter's key there, or it
/// now acts for another user, the key is revoked and the skip answers
/// [`ApiError::NoHandoff`], or [`ApiError::Internal`] if storing the revoke failed. In each
/// of these cases nothing is sent to the Group and nothing written.
#[post(
    "/api/groups/skip",
    headers: dioxus_fullstack::HeaderMap,
    bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>
)]
pub async fn skip_group(group: String, skip: Skip) -> Result<Playback, ApiError> {
    bridge
        .skip_group(server::session_token(&headers)?, group, skip)
        .await
}

/// Sets Group `group`'s volume, with or without a Handoff; a volume above 100 sets 100.
/// Answers with the volume set.
#[post(
    "/api/groups/volume",
    headers: dioxus_fullstack::HeaderMap,
    bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>
)]
pub async fn set_group_volume(group: String, volume: u8) -> Result<u8, ApiError> {
    bridge
        .set_group_volume(server::session_token(&headers)?, group, volume)
        .await
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

    use crate::{
        ApiError, ApiKey, CurrentUser, EpisodeFilter, Group, Handoff, Home, Item, ItemId, Library,
        LibraryEntry, LibraryServer, Playback, Podcast, Session, SessionToken, Skip,
    };

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

        /// See [`crate::libraries`].
        fn libraries(&self, token: SessionToken) -> BoxFuture<'_, Result<Vec<Library>, ApiError>>;

        /// See [`crate::library_entries`].
        fn library_entries(
            &self,
            token: SessionToken,
            library: String,
        ) -> BoxFuture<'_, Result<Vec<LibraryEntry>, ApiError>>;

        /// See [`crate::item`].
        fn item(&self, token: SessionToken, id: ItemId) -> BoxFuture<'_, Result<Item, ApiError>>;

        /// See [`crate::mark_finished`].
        fn mark_finished(
            &self,
            token: SessionToken,
            item: ItemId,
            finished: bool,
        ) -> BoxFuture<'_, Result<Item, ApiError>>;

        /// See [`crate::home`].
        fn home(&self, token: SessionToken) -> BoxFuture<'_, Result<Home, ApiError>>;

        /// See [`crate::podcast`].
        fn podcast(
            &self,
            token: SessionToken,
            podcast: String,
            filter: EpisodeFilter,
        ) -> BoxFuture<'_, Result<Podcast, ApiError>>;

        /// See [`crate::groups`].
        fn groups(&self, token: SessionToken) -> BoxFuture<'_, Result<Vec<Group>, ApiError>>;

        /// See [`crate::start_handoff`].
        fn start_handoff(
            &self,
            token: SessionToken,
            item: ItemId,
            group: String,
        ) -> BoxFuture<'_, Result<Handoff, ApiError>>;

        /// See [`crate::handoffs`].
        fn handoffs(&self, token: SessionToken) -> BoxFuture<'_, Result<Vec<Handoff>, ApiError>>;

        /// See [`crate::pause_group`].
        fn pause_group(
            &self,
            token: SessionToken,
            group: String,
        ) -> BoxFuture<'_, Result<Playback, ApiError>>;

        /// See [`crate::resume_group`].
        fn resume_group(
            &self,
            token: SessionToken,
            group: String,
        ) -> BoxFuture<'_, Result<Playback, ApiError>>;

        /// See [`crate::skip_group`].
        fn skip_group(
            &self,
            token: SessionToken,
            group: String,
            skip: Skip,
        ) -> BoxFuture<'_, Result<Playback, ApiError>>;

        /// See [`crate::set_group_volume`].
        fn set_group_volume(
            &self,
            token: SessionToken,
            group: String,
            volume: u8,
        ) -> BoxFuture<'_, Result<u8, ApiError>>;
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

    use super::{ApiError, ApiKey, SessionToken, TransportState};

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
    fn a_transport_state_from_a_newer_bridge_decodes_as_unknown() {
        assert_eq!(
            serde_json::from_value::<TransportState>(json!("recording")).unwrap(),
            TransportState::Unknown
        );
        assert_eq!(
            serde_json::from_value::<TransportState>(json!("paused")).unwrap(),
            TransportState::Paused
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
    fn an_answer_without_an_app_api_code_is_a_gateway_failure_or_not_a_bridge() {
        use dioxus::fullstack::ServerFnError;
        let without_code = |code| ServerFnError::ServerError {
            message: format!("HTTP {code}: <html>"),
            code,
            details: None,
        };
        for code in [502, 503, 504] {
            assert_eq!(
                ApiError::from(without_code(code)),
                ApiError::BridgeUnreachable,
                "{code}"
            );
        }
        for code in [200, 404, 405, 422, 500, 501, 505] {
            assert_eq!(
                ApiError::from(without_code(code)),
                ApiError::NotABridge,
                "{code}"
            );
        }
        assert_eq!(
            ApiError::from(ServerFnError::Deserialization("<html>".into())),
            ApiError::NotABridge
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
