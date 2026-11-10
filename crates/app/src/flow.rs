//! The App's state and flow, apart from rendering: which screen shows, what it shows, and what
//! each action does, through the App API on the person's Bridge and the [`DeviceStore`].
//!
//! The UI renders [`App::screen`] and calls the actions; tests drive the same actions against
//! a real Bridge.
//!
//! Server functions reach the Bridge at a process-wide base URL and send a process-wide
//! `Authorization` header (through the App's client, installed in Dioxus fullstack's slot,
//! which follows no redirect: a call reaches the stored address only); [`App`] sets both, the
//! header to the selected session's token (except while ending another session, see
//! `App::end_session`), so one process runs one [`App`] at a time.

use std::sync::Arc;
use std::time::Duration;

use api::{
    ApiError, ApiKey, Episode, EpisodeFilter, Group, Home, Item, ItemId, Library, LibraryEntry,
    LibraryKind, LibraryServer, Playback, Podcast, Session, SessionToken, Skip,
};
use clock::Clock;
use dioxus::fullstack::http::header::{AUTHORIZATION, HeaderValue};
use dioxus::fullstack::{HeaderMap, get_server_url, set_request_headers, set_server_url};
use tokio::sync::mpsc;

use crate::client;
use crate::i18n::{self, Language, LanguageChoice, Text};
use crate::local::{Halted, LocalPlayback, LocalPlayer, LocalStatus, PlayerEvent};
use crate::storage::{DeviceStore, LoadError, Stored, StoredSession};

/// What the App shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    /// Checking a stored session with the Bridge.
    Starting,
    /// Asks for the Bridge's address.
    BridgeUrl { notice: Option<Notice> },
    /// Picks a Library Server and takes an API key; also offers the Library Servers with a
    /// session to switch to, and Back when opened from Settings.
    Login {
        library_servers: Vec<LibraryServer>,
        notice: Option<Notice>,
    },
    /// Logged in, where a login lands: the Items the user is in the middle of and the newest
    /// episodes they haven't Finished, on the session's Library Server. `home` is `None` when
    /// Home could not be read (Try again offered); the notice saying why goes once the screen
    /// is left, as on any screen.
    Home {
        home: Option<Home>,
        notice: Option<Notice>,
    },
    /// Every Library the user can see on the session's Library Server, audiobooks and
    /// podcasts; opened from Home.
    Libraries {
        libraries: Vec<Library>,
        notice: Option<Notice>,
    },
    /// The audiobooks or podcasts in one Library.
    Library {
        library: Library,
        entries: Vec<LibraryEntry>,
        notice: Option<Notice>,
    },
    /// A podcast's episodes, as the filter the Bridge applied (`podcast.filter`) picked them.
    Podcast {
        podcast: Podcast,
        notice: Option<Notice>,
    },
    /// One Item: its details, the person's Progress, and an audiobook's chapters.
    Item { item: Item, notice: Option<Notice> },
    /// Picks the Group to play an Item on: every Group; none when the Bridge found no Sonos
    /// player. The Item starts at the person's Progress, or from its beginning when
    /// `from_start` ("Play from start").
    PlayOn {
        item: Item,
        groups: Vec<Group>,
        from_start: bool,
        notice: Option<Notice>,
    },
    /// Logged in: every Sonos Group with its members, what it plays, its volume and its
    /// Handoff, whose controls it offers. Empty, with a notice, when they could not be listed.
    Groups {
        groups: Vec<Group>,
        notice: Option<Notice>,
    },
    /// Logged in: the Library Server, the account, the language, logout, the other Library
    /// Servers with a session to switch to, and logging in to another Library Server.
    Settings { notice: Option<Notice> },
    /// Local playback: what plays on the phone, with its controls
    /// ([`App::local_status`]); opened by starting it, and from any logged-in screen while
    /// it runs. Leaving it keeps playback going.
    NowPlaying { notice: Option<Notice> },
}

impl Screen {
    /// The screen showing `podcast`'s episodes, without a notice.
    fn podcast(podcast: Podcast) -> Self {
        Self::Podcast {
            podcast,
            notice: None,
        }
    }

    /// What the screen tells the person about the last action, if anything.
    pub fn notice(&self) -> Option<&Notice> {
        match self {
            Self::Starting => None,
            Self::BridgeUrl { notice }
            | Self::Login { notice, .. }
            | Self::Home { notice, .. }
            | Self::Libraries { notice, .. }
            | Self::Library { notice, .. }
            | Self::Podcast { notice, .. }
            | Self::Item { notice, .. }
            | Self::PlayOn { notice, .. }
            | Self::Groups { notice, .. }
            | Self::Settings { notice }
            | Self::NowPlaying { notice } => notice.as_ref(),
        }
    }

    fn notice_mut(&mut self) -> Option<&mut Option<Notice>> {
        match self {
            Self::Starting => None,
            Self::BridgeUrl { notice }
            | Self::Login { notice, .. }
            | Self::Home { notice, .. }
            | Self::Libraries { notice, .. }
            | Self::Library { notice, .. }
            | Self::Podcast { notice, .. }
            | Self::Item { notice, .. }
            | Self::PlayOn { notice, .. }
            | Self::Groups { notice, .. }
            | Self::Settings { notice }
            | Self::NowPlaying { notice } => Some(notice),
        }
    }
}

/// Something the person should know about the last action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// The App API refused or failed.
    Api(ApiError),
    /// What was entered is not a Bridge address.
    BridgeUrlInvalid,
    /// Nothing answered at the address entered on the Bridge URL screen, where a typo is the
    /// likeliest cause.
    BridgeUrlUnreachable,
    /// The session's Library Server is not configured on the Bridge (any more); the session
    /// stays, as it may work again once the operator re-adds it.
    SessionLibraryServerNotConfigured,
    /// What the App saved on the device could not be read; it was moved aside and the App
    /// started afresh.
    StorageLoad,
    /// What the App saved on the device could neither be read nor moved aside; the App
    /// leaves it untouched and saves nothing during this launch.
    StorageStuck,
    /// The device storage could not be written.
    StorageSave,
    /// Logged out, but the device storage still holds the session.
    LogoutNotSaved,
    /// The Item now plays on the Group with this name.
    PlayingOn(String),
    /// This phone's player couldn't play the Item, so Local playback ended.
    LocalPlayerFailed,
    /// Where the Item played on this phone before stopped wasn't written as Progress, for
    /// this reason; shown on the next Item's Now playing.
    LocalEndNotSaved(ApiError),
}

impl Notice {
    /// Whether it reports something done, rather than a problem.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::PlayingOn(_))
    }

    /// The notice as the person reads it in `language`.
    pub fn text(&self, language: Language) -> String {
        let text = match self {
            Self::Api(error) => error.into(),
            Self::BridgeUrlInvalid => Text::ErrorBridgeUrlInvalid,
            Self::BridgeUrlUnreachable => Text::ErrorBridgeUrlUnreachable,
            Self::SessionLibraryServerNotConfigured => Text::ErrorSessionLibraryServerNotConfigured,
            Self::StorageLoad => Text::ErrorStorageLoad,
            Self::StorageStuck => Text::ErrorStorageStuck,
            Self::StorageSave => Text::ErrorStorageSave,
            Self::LogoutNotSaved => Text::ErrorLogoutNotSaved,
            Self::LocalPlayerFailed => Text::ErrorLocalPlayerFailed,
            Self::PlayingOn(group) => return i18n::playing_on(language, group),
            Self::LocalEndNotSaved(error) => return i18n::local_end_not_saved(language, error),
        };
        i18n::text(language, text)
    }
}

/// How long a call to the Bridge may take before the App treats the Bridge as unreachable.
/// Longer than the Bridge's own 10-second limit for a Library Server request (`abs::TIMEOUT`),
/// so a slow Library Server shows as `library_server_unreachable` rather than as the Bridge
/// being unreachable, and a login answered late isn't dropped; and than a Group control's or
/// a start's own limit of about 14 seconds plus local storage (a control: 3 waiting for its
/// turn on the Group, 4 for the Group, 7 for the Library Server).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// The App: what it remembers on the device and the screen it shows.
#[derive(Debug, Clone)]
pub struct App {
    store: Arc<dyn DeviceStore>,
    stored: Stored,
    device_language: Language,
    screen: Screen,
    /// The screens to go back to from the one shown, the last one first.
    back: Vec<Screen>,
    /// Why the stored state could not be loaded, for the first screen.
    load_notice: Option<Notice>,
    /// Set when what was saved could neither be read nor moved aside: saving would destroy it.
    read_only: bool,
    /// Sessions `log_in` replaced, each with the Bridge address that issued it, kept while the
    /// file may still hold them: a failed save leaves them there, usable. Each successful save
    /// (read-only: each save attempt) moves them to `unheld`. Never saved.
    replaced: Vec<(String, SessionToken)>,
    /// Sessions `log_in` replaced that the device no longer holds, to end on the Bridge that
    /// issued them at once, or, while Local playback runs, once it has stopped and a Stop, a
    /// halt shown, or a save follows ([`App::end_replaced`]); dropped unsent if the stored
    /// Bridge address differs by then.
    /// Never saved.
    unheld: Vec<(String, SessionToken)>,
    call_timeout: Duration,
    /// Local playback, where the App has a [`LocalPlayer`]; none elsewhere, which then offers
    /// no Local playback.
    local: Option<LocalPlayback>,
}

impl App {
    /// The App as it launches: what `store` remembers, on [`Screen::Starting`] until
    /// [`App::start`]. `device_locale` is the device's language (a BCP 47 tag such as `de-DE`).
    pub fn open(store: Arc<dyn DeviceStore>, device_locale: Option<&str>) -> Self {
        client::install();
        let (mut stored, load_notice, read_only) = match store.load() {
            Ok(stored) => (stored, None, false),
            Err(error @ LoadError::SetAside { .. }) => {
                tracing::warn!(%error, "starting afresh");
                (Stored::default(), Some(Notice::StorageLoad), false)
            }
            Err(error @ LoadError::Stuck { .. }) => {
                tracing::warn!(%error, "starting afresh; saving nothing during this launch");
                (Stored::default(), Some(Notice::StorageStuck), true)
            }
        };
        // A session belongs to its Bridge; without one it can't be used.
        if stored.bridge_url.is_none() {
            stored.forget_all();
        }
        if let Some(url) = &stored.bridge_url {
            point_at(url);
        }
        authorize_selected(&stored);
        Self {
            store,
            stored,
            device_language: Language::for_locale(device_locale),
            screen: Screen::Starting,
            back: Vec::new(),
            load_notice,
            read_only,
            replaced: Vec::new(),
            unheld: Vec::new(),
            call_timeout: CALL_TIMEOUT,
            local: None,
        }
    }

    /// This App with calls to the Bridge bounded by `timeout` instead of [`CALL_TIMEOUT`].
    pub fn with_call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// This App offering Local playback through `player`, whose events come through `events`,
    /// reporting on `clock`'s time. Its calls to the Bridge are bounded by the call timeout set
    /// so far. Must run within a Tokio runtime.
    pub fn with_local_player(
        mut self,
        player: Arc<dyn LocalPlayer>,
        events: mpsc::UnboundedReceiver<PlayerEvent>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        self.local = Some(LocalPlayback::new(player, events, clock, self.call_timeout));
        self
    }

    /// Picks the first screen: the Bridge address if there is none, Login if no session is
    /// selected, else Home once the Bridge confirmed the selected session (only that
    /// one is checked). A session whose key the Library Server rejected (`api_key_rejected`) is
    /// forgotten; so is one the Bridge reports as `session_invalid` while its Library Server is
    /// still configured. On any other failure (the Bridge or Library Server unreachable, the
    /// Library Server not configured for now, for example) it stays, and Settings shows why.
    pub async fn start(&mut self) {
        let mut notice = self.load_notice.take();
        if self.stored.bridge_url.is_none() {
            self.screen = Screen::BridgeUrl { notice };
            return;
        }
        if self.stored.session().is_none() {
            self.show_login(notice).await;
            return;
        }
        match self.call(api::current_user()).await {
            Ok(user) => {
                if self.stored.update_user(user) {
                    notice = notice.or(self.save().await);
                }
                self.show_home(notice).await;
            }
            Err(error) => {
                if let Some(error) = self.session_call_failed(error).await {
                    self.screen = Screen::Settings {
                        notice: Some(Notice::Api(error)),
                    };
                }
            }
        }
    }

    /// Connects to the Bridge at `input` and remembers it if it answers with its Library
    /// Servers. An address that redirects to a Bridge, keeping the path it was asked for, is
    /// remembered as the Bridge's address, so later calls (which follow no redirect) go there
    /// directly; one that redirects elsewhere (say to a bare `https://host/`) is rejected. Only
    /// while no session is stored: a session belongs to its Bridge.
    pub async fn submit_bridge_url(&mut self, input: &str) {
        if !self.can_change_bridge_url() {
            return;
        }
        let Some(mut url) = bridge_url(input) else {
            self.screen = Screen::BridgeUrl {
                notice: Some(Notice::BridgeUrlInvalid),
            };
            return;
        };
        let previous = get_server_url();
        point_at(&url);
        // Unlike other calls, the person can fix an address that isn't a Bridge right here.
        let mut answer = self.probe(api::library_servers()).await;
        // A redirect answers as something other than a Bridge.
        if matches!(answer, Err(ApiError::NotABridge)) {
            match self.redirected_bridge_url(&url).await {
                Ok(Some(redirected)) => {
                    url = redirected;
                    point_at(&url);
                    answer = self.probe(api::library_servers()).await;
                }
                Ok(None) => {}
                Err(client::Unreachable) => answer = Err(ApiError::BridgeUnreachable),
            }
        }
        match answer {
            Ok(library_servers) => {
                self.stored.bridge_url = Some(url);
                self.screen = Screen::Login {
                    library_servers,
                    notice: self.save().await,
                };
            }
            Err(error) => {
                set_server_url(previous);
                let notice = match error {
                    ApiError::BridgeUnreachable => Notice::BridgeUrlUnreachable,
                    error => Notice::Api(error),
                };
                self.screen = Screen::BridgeUrl {
                    notice: Some(notice),
                };
            }
        }
    }

    /// The Bridge address the address `url` redirects to, if it does: where asking `url` for
    /// its Library Servers lands, without their path. Bounded by the call timeout; a redirect
    /// target that can't be reached in time is [`client::Unreachable`].
    async fn redirected_bridge_url(
        &self,
        url: &str,
    ) -> Result<Option<String>, client::Unreachable> {
        let Ok(asked) = url::Url::parse(&format!("{url}{LIBRARY_SERVERS_PATH}")) else {
            return Ok(None);
        };
        let target = tokio::time::timeout(self.call_timeout, client::redirect_target(asked))
            .await
            .map_err(|_| {
                tracing::debug!("following the address's redirects timed out");
                client::Unreachable
            })??;
        Ok(target.as_ref().and_then(bridge_url_at))
    }

    /// Back to the Bridge address, to enter another one. Only while no session is stored.
    pub fn change_bridge_url(&mut self) {
        if self.can_change_bridge_url() {
            self.screen = Screen::BridgeUrl { notice: None };
        }
    }

    /// Asks the Bridge for its Library Servers again, for Login.
    pub async fn reload_library_servers(&mut self) {
        if matches!(self.screen, Screen::Login { .. }) && self.stored.bridge_url.is_some() {
            self.show_login(None).await;
        }
    }

    /// Logs in to the Library Server with base URL `library_server`, adding its session and
    /// selecting it; the other sessions stay. A session already on that Library Server is
    /// replaced, and ended on the Bridge as far as it can once the device no longer holds it:
    /// after this save or, if that fails, the next one that succeeds (read-only: at once); and,
    /// should Local playback run by then, once it has stopped and a Stop, a halt shown, or a
    /// save follows ([`App::end_replaced`]). It only ever reaches the Bridge that issued it:
    /// dropped unsent if the stored address differs by then. The key is sent to the Bridge and
    /// not kept.
    pub async fn log_in(&mut self, library_server: &str, api_key: ApiKey) {
        if !matches!(self.screen, Screen::Login { .. }) {
            return;
        }
        // Pasted keys often bring a space or line break along.
        let api_key = ApiKey::new(api_key.expose().trim());
        match self
            .call(api::login(library_server.to_owned(), api_key))
            .await
        {
            Ok(Session { token, user }) => {
                // Local playback belongs to the session that started it.
                self.end_local_playback().await;
                let listed = match &self.screen {
                    Screen::Login {
                        library_servers, ..
                    } => library_servers
                        .iter()
                        .find(|listed| listed.base_url == library_server)
                        .cloned(),
                    _ => None,
                };
                let library_server = listed.unwrap_or_else(|| LibraryServer {
                    base_url: user.library_server.clone(),
                    name: None,
                });
                let replaced = self.stored.add(StoredSession {
                    token,
                    library_server,
                    user,
                });
                authorize_selected(&self.stored);
                if let Some(replaced) = replaced
                    && let Some(bridge_url) = &self.stored.bridge_url
                {
                    // `save` ends it once the device no longer holds it.
                    self.replaced.push((bridge_url.clone(), replaced.token));
                }
                let notice = self.save().await;
                self.show_home(notice).await;
            }
            Err(error) => self.set_notice(Notice::Api(error)),
        }
    }

    /// Ends the selected session on the Bridge and forgets it on the device, also when the
    /// Bridge can't be reached. The other sessions stay; Login offers them.
    pub async fn log_out(&mut self) {
        let Some(session) = self.stored.session() else {
            return;
        };
        self.end_local_playback().await;
        self.end_session(&session.token).await;
        let not_saved = self.forget_session().await;
        self.show_login(None).await;
        match not_saved {
            // The token is still on the device: the next launch may be logged in again.
            Some(Notice::StorageSave) => self.set_notice(Notice::LogoutNotSaved),
            Some(notice) => self.set_notice(notice),
            None => {}
        }
    }

    /// Home, read afresh: from Groups or Settings, or to try again after a failure. The
    /// screens to go back to are dropped.
    pub async fn open_home(&mut self) {
        if self.stored.session().is_some() {
            self.show_home(None).await;
        }
    }

    /// From Home, every Library the user can see; Back returns to Home.
    pub async fn open_libraries(&mut self) {
        self.go(api::libraries(), |libraries| Screen::Libraries {
            libraries,
            notice: None,
        })
        .await;
    }

    /// Selects the session on the Library Server with base URL `library_server`, without a
    /// login, and shows its Home: from Settings or Login. Every later call acts on
    /// that session; once saved, the next launch starts with it.
    pub async fn switch_to(&mut self, library_server: &str) {
        if !self.stored.select(library_server) {
            return;
        }
        // Calls still carry the session switched away from: its Local playback ends with it.
        self.end_local_playback().await;
        authorize_selected(&self.stored);
        let notice = self.save().await;
        self.show_home(notice).await;
    }

    /// From Settings, Login with the Bridge's Library Servers, to log in to another one; Back
    /// returns to Settings.
    pub async fn open_login(&mut self) {
        self.go(api::library_servers(), |library_servers| Screen::Login {
            library_servers,
            notice: None,
        })
        .await;
    }

    /// Opens `library`, listing its audiobooks or podcasts.
    pub async fn open_library(&mut self, library: Library) {
        let call = api::library_entries(library.id.clone());
        self.go(call, |entries| Screen::Library {
            library,
            entries,
            notice: None,
        })
        .await;
    }

    /// Opens entry `id` of the Library shown: an audiobook's Item, or a podcast's episodes.
    pub async fn open_entry(&mut self, id: &str) {
        let Screen::Library { library, .. } = &self.screen else {
            return;
        };
        match library.kind {
            LibraryKind::Audiobooks => {
                let item = ItemId {
                    library_item: id.to_owned(),
                    episode: None,
                };
                self.open_item(item).await;
            }
            LibraryKind::Podcasts => self.open_podcast(id).await,
        }
    }

    /// Opens podcast `id`, listing its unfinished episodes.
    pub async fn open_podcast(&mut self, id: &str) {
        let call = api::podcast(id.to_owned(), Some(EpisodeFilter::default()));
        self.go(call, Screen::podcast).await;
    }

    /// Lists the episodes `filter` picks, on the podcast shown.
    pub async fn filter_episodes(&mut self, filter: EpisodeFilter) {
        let Screen::Podcast { podcast, .. } = &self.screen else {
            return;
        };
        match self
            .call(api::podcast(podcast.id.clone(), Some(filter)))
            .await
        {
            Ok(podcast) => self.screen = Screen::podcast(podcast),
            Err(error) => self.failed(error).await,
        }
    }

    /// Opens episode `id` of the podcast shown.
    pub async fn open_episode(&mut self, id: &str) {
        let Screen::Podcast { podcast, .. } = &self.screen else {
            return;
        };
        let item = ItemId {
            library_item: podcast.id.clone(),
            episode: Some(id.to_owned()),
        };
        self.open_item(item).await;
    }

    /// Opens the Item `id`.
    pub async fn open_item(&mut self, id: ItemId) {
        self.go(api::item(id.library_item, id.episode), |item| {
            Screen::Item { item, notice: None }
        })
        .await;
    }

    /// Whether the Item screen offers to play its Item on a Group: only an Item in one audio
    /// file plays there so far.
    pub fn can_play_on(&self) -> bool {
        matches!(&self.screen, Screen::Item { item, .. } if item.audio_files.len() == 1)
    }

    /// On the Item screen, marks its Item Finished (`finished`) or unfinished on the Library
    /// Server, and shows the Item as the Bridge then answers; the podcast screen to go back to
    /// lists the episode by its new state, and Home to go back to is read again (if that read
    /// fails without ending the session, Home stays as it was, saying nothing of it). When the
    /// Library Server can't be reached, the Bridge may have written before reading the Progress
    /// back failed: the Item, its podcast's episodes and Home are read again, as far as that
    /// works. On any other failure that keeps the session the screen stays as shown. Either way
    /// it says why; a mark whose answer was lost (the App gave up waiting, for example) may
    /// still land on the Library Server, which opening the Item again shows.
    pub async fn mark_finished(&mut self, finished: bool) {
        let Screen::Item { item, .. } = &self.screen else {
            return;
        };
        let id = item.id.clone();
        match self.call(api::mark_finished(id.clone(), finished)).await {
            Ok(item) => {
                for screen in &mut self.back {
                    if let Screen::Podcast { podcast, .. } = screen {
                        relist_episode(podcast, &item);
                    }
                }
                self.screen = Screen::Item { item, notice: None };
                self.reread_home().await;
            }
            Err(error @ ApiError::LibraryServerUnreachable) => {
                if self.reread(&id).await {
                    self.set_notice(Notice::Api(error));
                }
            }
            Err(error) => self.failed(error).await,
        }
    }

    /// Shows Item `id` on its screen as the Library Server holds it now, then (once that
    /// worked) its podcast's episodes and Home on the screens to go back to; whatever can't be
    /// read again stays as shown. Returns whether the session goes on: if not, the screen shown
    /// says why.
    async fn reread(&mut self, id: &ItemId) -> bool {
        match self
            .call(api::item(id.library_item.clone(), id.episode.clone()))
            .await
        {
            Ok(item) => self.screen = Screen::Item { item, notice: None },
            Err(error) => return self.reread_failed(error).await,
        }
        for at in 0..self.back.len() {
            let Screen::Podcast { podcast, .. } = &self.back[at] else {
                continue;
            };
            if podcast.id != id.library_item {
                continue;
            }
            match self
                .call(api::podcast(podcast.id.clone(), Some(podcast.filter)))
                .await
            {
                Ok(listed) => {
                    if let Screen::Podcast { podcast, .. } = &mut self.back[at] {
                        *podcast = listed;
                    }
                }
                Err(error) => return self.reread_failed(error).await,
            }
        }
        self.reread_home().await
    }

    /// Reads Home again on the screens to go back to, if it is one of them, after a mark
    /// changed what it lists; if that read fails, Home stays as it was. Returns whether the
    /// session goes on: if not, the screen shown says why.
    async fn reread_home(&mut self) -> bool {
        let Some(at) = self
            .back
            .iter()
            .position(|screen| matches!(screen, Screen::Home { .. }))
        else {
            return true;
        };
        match self.call(api::home()).await {
            Ok(home) => {
                self.back[at] = Screen::Home {
                    home: Some(home),
                    notice: None,
                };
                true
            }
            Err(error) => self.reread_failed(error).await,
        }
    }

    /// Handles a failure to read again after a mark: one that concerns the session goes to
    /// [`App::session_call_failed`]; any other is only logged. Returns whether the session
    /// goes on.
    async fn reread_failed(&mut self, error: ApiError) -> bool {
        match self.session_call_failed(error).await {
            Some(error) => {
                tracing::debug!(%error, "cannot read again after a mark");
                true
            }
            None => false,
        }
    }

    /// From the Item screen, lists every Group to play the Item on, at the person's Progress.
    pub async fn open_play_on(&mut self) {
        self.open_picker(false).await;
    }

    /// From the Item screen, lists every Group to play the Item on from its beginning.
    pub async fn open_play_from_start(&mut self) {
        self.open_picker(true).await;
    }

    /// From the Item screen, lists every Group to play the Item on, from its beginning if
    /// `from_start`.
    async fn open_picker(&mut self, from_start: bool) {
        let Screen::Item { item, .. } = &self.screen else {
            return;
        };
        let item = item.clone();
        self.go(api::groups(), |groups| Screen::PlayOn {
            item,
            groups,
            from_start,
            notice: None,
        })
        .await;
    }

    /// On the Group picker, starts the Item on Group `group` (its id), at the person's
    /// Progress or from its beginning, as the picker was opened for, and goes back to the
    /// Item, noting where it plays; or shows why it could not start. A Group that is gone
    /// (`group_not_found`) has the picker list the Groups afresh, with that notice; if they
    /// can't be listed, the picker keeps its list and shows why instead (an ended session
    /// leads to Login).
    pub async fn play_on(&mut self, group: &str) {
        let Screen::PlayOn {
            item,
            groups,
            from_start,
            ..
        } = &self.screen
        else {
            return;
        };
        let (id, from_start) = (item.id.clone(), *from_start);
        let name = groups
            .iter()
            .find(|listed| listed.id == group)
            .map_or_else(|| group.to_owned(), |listed| listed.name.clone());
        match self
            .call(api::start_handoff(id, group.to_owned(), Some(from_start)))
            .await
        {
            Ok(_) => {
                // Back to the Item screen the picker was opened from.
                self.go_back();
                self.set_notice(Notice::PlayingOn(name));
            }
            Err(error @ ApiError::GroupNotFound) => self.relist_groups(error).await,
            Err(error) => self.failed(error).await,
        }
    }

    /// Whether the Item screen offers to play its Item on this phone: only where the App has a
    /// player (iOS), and for an Item that plays on a Group too.
    pub fn can_play_on_phone(&self) -> bool {
        self.local.is_some() && self.can_play_on()
    }

    /// On the Item screen, plays its Item on this phone from the person's Progress, and opens
    /// Now playing, to come back from to the Item; or shows why it could not start. Whatever
    /// played on the phone ends first, its position written as Progress (as does an end still
    /// on its way); if that write failed, the new Now playing says so
    /// ([`Notice::LocalEndNotSaved`]). A player failure not yet shown is only logged, as is
    /// the end after it should that have failed too, unless it ended the session: the Item it
    /// was about no longer plays.
    pub async fn play_on_phone(&mut self) {
        let (Some(local), Some(bridge_url), Screen::Item { item, .. }) =
            (&self.local, &self.stored.bridge_url, &self.screen)
        else {
            return;
        };
        let (local, bridge_url, id) = (local.clone(), bridge_url.clone(), item.id.clone());
        let ended = match (local.stop().await, local.take_halted()) {
            (Err(error), _) | (Ok(()), Some(Halted::Api(error))) => {
                match self.session_call_failed(error).await {
                    Some(error) => Some(Notice::LocalEndNotSaved(error)),
                    None => return,
                }
            }
            (Ok(()), Some(Halted::PlayerFailed)) => {
                tracing::info!("the player failed on the Item before; not said on the next one");
                None
            }
            (Ok(()), None) => None,
        };
        match self.call(api::start_local(id)).await {
            Ok(started) => match local.start(started, &bridge_url).await {
                Ok(()) => self.push_screen(Screen::NowPlaying { notice: ended }),
                Err(error) => self.set_notice(Notice::Api(error)),
            },
            // Why nothing plays matters more.
            Err(error) => self.failed(error).await,
        }
    }

    /// What plays on this phone, if anything.
    pub fn local_status(&self) -> Option<LocalStatus> {
        self.local.as_ref()?.status()
    }

    /// The notice Now playing shows, while it shows: the action's own first, but a failed
    /// report of the Item playing says why over a carried [`Notice::LocalEndNotSaved`], until
    /// a report succeeds.
    pub fn now_playing_notice(&self) -> Option<Notice> {
        let Screen::NowPlaying { notice } = &self.screen else {
            return None;
        };
        let report_failed = self.local_status().and_then(|status| status.report_failed);
        match (notice, report_failed) {
            (None | Some(Notice::LocalEndNotSaved(_)), Some(error)) => Some(Notice::Api(error)),
            (notice, _) => notice.clone(),
        }
    }

    /// Opens Now playing from any logged-in screen while something plays on this phone, to
    /// come back from to that screen.
    pub fn open_now_playing(&mut self) {
        let playing = self.local.as_ref().is_some_and(LocalPlayback::is_active);
        if playing
            && self.stored.session().is_some()
            && !matches!(self.screen, Screen::NowPlaying { .. })
        {
            self.push_screen(Screen::NowPlaying { notice: None });
        }
    }

    /// On Now playing, pauses (writing where as Progress) or plays on. The player acts at
    /// once; how the report went shows on Now playing, or through
    /// [`App::show_local_halted`] if it ended Local playback.
    pub async fn toggle_local(&mut self) {
        if let Some(local) = self.local_on_screen()
            && let Err(error) = local.toggle().await
        {
            self.set_notice(Notice::Api(error));
        }
    }

    /// On Now playing, moves by `skip` (writing where to as Progress), as
    /// [`App::toggle_local`] does.
    pub async fn skip_local(&mut self, skip: Skip) {
        if let Some(local) = self.local_on_screen()
            && let Err(error) = local.skip(skip).await
        {
            self.set_notice(Notice::Api(error));
        }
    }

    /// On Now playing, ends Local playback, writing where it stopped as Progress, and goes
    /// back to the screen Now playing was opened from, saying if that write failed. A write
    /// that tells the session ended leads to Login instead, as for any call. Sessions `log_in`
    /// replaced that waited for Local playback to end are ended then ([`App::end_replaced`]).
    pub async fn stop_local(&mut self) {
        let Some(local) = self.local_on_screen() else {
            return;
        };
        let answer = local.stop().await;
        if self.local_answered(answer.clone()).await {
            self.go_back();
            if let Err(error) = answer {
                self.set_notice(Notice::Api(error));
            }
            self.end_replaced().await;
        }
    }

    /// Whether Local playback stopped on its own (the Bridge ended its Local session, or the
    /// player failed) and the App hasn't said why yet ([`App::show_local_halted`]).
    pub fn local_halted(&self) -> bool {
        self.local.as_ref().is_some_and(LocalPlayback::has_halted)
    }

    /// Whether why Local playback stopped on its own, not yet said, ends the session; a probe
    /// for tests that leaves it for [`App::show_local_halted`].
    #[doc(hidden)]
    pub fn local_halt_ends_session(&self) -> bool {
        self.local
            .as_ref()
            .is_some_and(LocalPlayback::halt_ends_session)
    }

    /// Says why Local playback stopped on its own, if it did since last asked: an ended
    /// session leads to Login, as for any call; any other reason shows as a notice on the
    /// screen shown, and sessions `log_in` replaced that waited for Local playback to end are
    /// ended then ([`App::end_replaced`]). The UI asks every second while no other action
    /// runs. Leaving a session drops what it hadn't said yet, logged, never shown to the next
    /// ([`App::end_local_playback`]).
    pub async fn show_local_halted(&mut self) {
        match self.local.as_ref().and_then(LocalPlayback::take_halted) {
            Some(Halted::Api(error)) => self.failed(error).await,
            Some(Halted::PlayerFailed) => self.set_notice(Notice::LocalPlayerFailed),
            None => return,
        }
        self.end_replaced().await;
    }

    /// Local playback, while Now playing shows.
    fn local_on_screen(&self) -> Option<LocalPlayback> {
        match self.screen {
            Screen::NowPlaying { .. } => self.local.clone(),
            _ => None,
        }
    }

    /// Handles `answer`, how ending Local playback went: an ended session leads to Login (as
    /// for any call). Returns whether the session goes on; any other failure is the caller's
    /// to show.
    async fn local_answered(&mut self, answer: Result<(), ApiError>) -> bool {
        match answer {
            Ok(()) => true,
            Err(error) => self.session_call_failed(error).await.is_some(),
        }
    }

    /// Ends Local playback, as far as the Bridge can be told, before the session it belongs to
    /// is left: what plays, and any end still on its way, all sent with that session's token.
    /// Why it stopped on its own, not yet said, is dropped: logged, never shown to the next
    /// session.
    async fn end_local_playback(&self) {
        let Some(local) = &self.local else {
            return;
        };
        if let Err(error) = local.stop().await {
            tracing::warn!(%error, "Local playback ended here; its last position isn't written");
        }
        if let Some(halted) = local.take_halted() {
            tracing::warn!(
                ?halted,
                "Local playback stopped on its own in the session left"
            );
        }
    }

    /// Opens Settings, from any logged-in screen; the screens to go back to are dropped (the
    /// UI offers it on Home).
    pub fn open_settings(&mut self) {
        if self.stored.session().is_some() {
            self.back.clear();
            self.screen = Screen::Settings { notice: None };
        }
    }

    /// Lists every Sonos Group with what it plays, afresh: from any logged-in screen (the UI
    /// offers it on Home), or to refresh the list. The screens to go back to
    /// are dropped.
    pub async fn open_groups(&mut self) {
        if self.stored.session().is_none() {
            return;
        }
        self.back.clear();
        match self.call(api::groups()).await {
            Ok(groups) => {
                self.screen = Screen::Groups {
                    groups,
                    notice: None,
                }
            }
            Err(error) => {
                if let Some(error) = self.session_call_failed(error).await {
                    self.screen = Screen::Groups {
                        groups: Vec::new(),
                        notice: Some(Notice::Api(error)),
                    };
                }
            }
        }
    }

    /// On the Groups screen, pauses Group `group`'s Handoff (whoever started it) and shows the
    /// Group paused where the Bridge says it stopped. On failure, as every Group control: a
    /// Group gone or without a Handoff it plays has the Groups listed afresh with that notice;
    /// anything else is said on the screen.
    pub async fn pause_group(&mut self, group: &str) {
        let call = api::pause_group(group.to_owned());
        self.control_group(group, call, show_playback).await;
    }

    /// On the Groups screen, has Group `group` play its Handoff on, and shows it playing.
    pub async fn resume_group(&mut self, group: &str) {
        let call = api::resume_group(group.to_owned());
        self.control_group(group, call, show_playback).await;
    }

    /// On the Groups screen, moves Group `group`'s Handoff by `skip`, and shows where it is
    /// then.
    pub async fn skip_group(&mut self, group: &str, skip: Skip) {
        let call = api::skip_group(group.to_owned(), skip);
        self.control_group(group, call, show_playback).await;
    }

    /// On the Groups screen, sets Group `group`'s volume to `volume` (at most 100), and shows
    /// the volume set.
    pub async fn set_group_volume(&mut self, group: &str, volume: u8) {
        let call = api::set_group_volume(group.to_owned(), volume);
        self.control_group(group, call, |group, volume| group.volume = Some(volume))
            .await;
    }

    /// On the Groups screen, makes `call` controlling Group `group`, and shows its answer on
    /// the Group's row with `shown`, clearing any notice. A Group that is gone
    /// (`group_not_found`) or no longer has a Handoff it plays (`no_handoff`) has the Groups
    /// listed afresh, with that notice; if they can't be listed, the list stays and says why
    /// instead (an ended session leads to Login). Any other failure is said on the screen as
    /// it is.
    async fn control_group<T>(
        &mut self,
        group: &str,
        call: impl Future<Output = Result<T, ApiError>>,
        shown: impl FnOnce(&mut Group, T),
    ) {
        if !matches!(self.screen, Screen::Groups { .. }) {
            return;
        }
        match self.call(call).await {
            Ok(answer) => {
                if let Screen::Groups { groups, notice } = &mut self.screen {
                    *notice = None;
                    if let Some(listed) = groups.iter_mut().find(|listed| listed.id == group) {
                        shown(listed, answer);
                    }
                }
            }
            Err(error @ (ApiError::GroupNotFound | ApiError::NoHandoff)) => {
                self.relist_groups(error).await;
            }
            Err(error) => self.failed(error).await,
        }
    }

    /// After `error` showed the Groups listed (on the picker or the Groups screen) are out of
    /// date, lists them afresh and says `error`; if they can't be listed, the list stays and
    /// says why instead (an ended session leads to Login).
    async fn relist_groups(&mut self, error: ApiError) {
        match self.call(api::groups()).await {
            Ok(listed) => {
                if let Screen::PlayOn { groups, .. } | Screen::Groups { groups, .. } =
                    &mut self.screen
                {
                    *groups = listed;
                }
                self.set_notice(Notice::Api(error));
            }
            // Why the Groups could not be listed afresh matters more now.
            Err(error) => self.failed(error).await,
        }
    }

    /// Whether [`App::go_back`] has a screen to go back to.
    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    /// Back to the screen the shown one was opened from, as it was apart from its notice,
    /// which was cleared on leaving it, and apart from a mark since, which may have updated it
    /// or read it again ([`App::mark_finished`]).
    pub fn go_back(&mut self) {
        if let Some(screen) = self.back.pop() {
            self.screen = screen;
        }
    }

    /// Switches the language now and remembers the choice. Saving may first end sessions
    /// `log_in` replaced earlier on the Bridge.
    pub async fn set_language(&mut self, choice: LanguageChoice) {
        self.stored.language = choice;
        match self.save().await {
            Some(notice) => self.set_notice(notice),
            // Everything is saved now, so an earlier failure no longer matters.
            None => {
                if let Some(notice) = self.screen.notice_mut()
                    && matches!(notice, Some(Notice::StorageSave | Notice::LogoutNotSaved))
                {
                    *notice = None;
                }
            }
        }
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// Shows Home with `notice`, or why it could not be read, which then replaces `notice`.
    /// The screens to go back to are dropped.
    async fn show_home(&mut self, notice: Option<Notice>) {
        self.back.clear();
        match self.call(api::home()).await {
            Ok(home) => {
                self.screen = Screen::Home {
                    home: Some(home),
                    notice,
                }
            }
            Err(error) => {
                if let Some(error) = self.session_call_failed(error).await {
                    self.screen = Screen::Home {
                        home: None,
                        notice: Some(Notice::Api(error)),
                    };
                }
            }
        }
    }

    /// While logged in, makes `call` and shows the screen `opened` makes of its answer, to come
    /// back from to the one shown now; or shows why it failed.
    async fn go<T>(
        &mut self,
        call: impl Future<Output = Result<T, ApiError>>,
        opened: impl FnOnce(T) -> Screen,
    ) {
        if self.stored.session().is_none() {
            return;
        }
        match self.call(call).await {
            Ok(answer) => self.push_screen(opened(answer)),
            Err(error) => self.failed(error).await,
        }
    }

    /// Shows `screen`, to come back from to the one shown now, whose notice goes.
    fn push_screen(&mut self, screen: Screen) {
        let mut left = std::mem::replace(&mut self.screen, screen);
        if let Some(notice) = left.notice_mut() {
            *notice = None;
        }
        self.back.push(left);
    }

    /// Shows why a call failed on the screen shown, unless it ended the session.
    async fn failed(&mut self, error: ApiError) {
        if let Some(error) = self.session_call_failed(error).await {
            self.set_notice(Notice::Api(error));
        }
    }

    /// Handles a session call's `error` that concerns the session: a rejected key forgets it
    /// and shows Login; `session_invalid` goes to [`App::session_invalid`]. Returns any other
    /// error, for the caller to show.
    async fn session_call_failed(&mut self, error: ApiError) -> Option<ApiError> {
        match error {
            ApiError::ApiKeyRejected => {
                // The revocation matters more than a failed save, which `save` logged.
                self.forget_session().await;
                self.show_login(Some(Notice::Api(error))).await;
                None
            }
            ApiError::SessionInvalid => {
                self.session_invalid().await;
                None
            }
            error => Some(error),
        }
    }

    /// The remembered Bridge address.
    pub fn bridge_url(&self) -> Option<&str> {
        self.stored.bridge_url.as_deref()
    }

    /// Whether the Bridge address may change: only while no session is stored, as a session
    /// belongs to its Bridge.
    pub fn can_change_bridge_url(&self) -> bool {
        !self.stored.has_sessions()
    }

    /// The selected session: its Library Server and account.
    pub fn session(&self) -> Option<&StoredSession> {
        self.stored.session()
    }

    /// The sessions other than the selected one, to switch to.
    pub fn other_sessions(&self) -> impl Iterator<Item = &StoredSession> {
        self.stored.other_sessions()
    }

    pub fn language_choice(&self) -> LanguageChoice {
        self.stored.language
    }

    /// The language the App shows now.
    pub fn language(&self) -> Language {
        self.stored.language.resolve(self.device_language)
    }

    /// `text` in [`App::language`].
    pub fn text(&self, text: Text) -> String {
        i18n::text(self.language(), text)
    }

    /// Shows Login with the Bridge's Library Servers and `notice`, or why they could not be
    /// listed, which then replaces `notice`.
    async fn show_login(&mut self, notice: Option<Notice>) {
        let (library_servers, notice) = match self.call(api::library_servers()).await {
            Ok(library_servers) => (library_servers, notice),
            Err(error) => (Vec::new(), Some(Notice::Api(error))),
        };
        self.screen = Screen::Login {
            library_servers,
            notice,
        };
    }

    /// After `session_invalid`: the session ended, or its Library Server is not configured on
    /// the Bridge, maybe only for now. Whether the Bridge still lists that Library Server tells
    /// the two apart; without the list, the session stays.
    async fn session_invalid(&mut self) {
        let notice = match self.call(api::library_servers()).await {
            Ok(library_servers) if self.lists_session_library_server(&library_servers) => {
                // The ended session matters more than a failed save, which `save` logged.
                self.forget_session().await;
                self.screen = Screen::Login {
                    library_servers,
                    notice: Some(Notice::Api(ApiError::SessionInvalid)),
                };
                return;
            }
            Ok(_) => Notice::SessionLibraryServerNotConfigured,
            Err(error) => Notice::Api(error),
        };
        self.screen = Screen::Settings {
            notice: Some(notice),
        };
    }

    /// Whether `library_servers` includes the selected session's Library Server.
    fn lists_session_library_server(&self, library_servers: &[LibraryServer]) -> bool {
        self.stored.session().is_some_and(|session| {
            library_servers
                .iter()
                .any(|listed| listed.base_url == session.library_server_url())
        })
    }

    /// Forgets the selected session on the device; the others stay. Returns a notice if that
    /// could not be saved.
    async fn forget_session(&mut self) -> Option<Notice> {
        // As far as the session still works, which it may not.
        self.end_local_playback().await;
        self.stored.forget_selected();
        self.back.clear();
        authorize_selected(&self.stored);
        self.save().await
    }

    /// Ends the session with `token` on the Bridge, as far as it can; callers forget it either
    /// way. Later calls send the selected session's token again.
    async fn end_session(&self, token: &SessionToken) {
        // The process-wide header carries `token` until the restore below, for every call: no
        // guard restores it if this future is dropped midway, which is safe while `act` runs
        // every action to its end, as the screen that started it stays until `act` sets the
        // result. Local playback's sender task calls on its own, so callers end Local playback
        // first (`log_out`), or put it off until Local playback has stopped, then wait for an
        // end on its way (`end_replaced`).
        authorize(Some(token));
        match self.call(api::logout()).await {
            Ok(()) => {}
            Err(error @ (ApiError::BridgeUnreachable | ApiError::SessionInvalid)) => {
                tracing::info!(%error, "the Bridge did not end the session; forgetting it anyway");
            }
            Err(error) => {
                tracing::warn!(%error, "the Bridge did not end the session; forgetting it anyway");
            }
        }
        authorize_selected(&self.stored);
    }

    /// Saves what the App remembers, then ends the sessions `log_in` replaced, which the device
    /// no longer holds ([`App::end_replaced`]). Returns a notice if saving failed; the
    /// replaced sessions stay then, unless the App is read-only.
    async fn save(&mut self) -> Option<Notice> {
        let notice = if self.read_only {
            // Not kept after a restart: the person needs to know with every change.
            tracing::debug!("not saving: the unreadable saved state stays untouched");
            // The file holds none of this launch's sessions, the replaced ones included.
            Some(Notice::StorageStuck)
        } else if let Err(error) = self.store.save(&self.stored) {
            tracing::warn!(%error, "cannot save the App's state on the device");
            return Some(Notice::StorageSave);
        } else {
            None
        };
        self.unheld.append(&mut self.replaced);
        self.end_replaced().await;
        notice
    }

    /// Ends the sessions `log_in` replaced that the device no longer holds, on the Bridge that
    /// issued them; those of another Bridge are dropped unsent. While Local playback runs,
    /// they wait until it has stopped and a Stop, a halt shown, or a save follows: its reports
    /// would go out with their token meanwhile.
    async fn end_replaced(&mut self) {
        if self.unheld.is_empty() {
            return;
        }
        if let Some(local) = &self.local {
            if local.is_active() {
                tracing::info!(
                    "Local playback runs: replaced sessions end once it has stopped and a Stop, \
                     a halt shown, or a save follows"
                );
                return;
            }
            // Nothing plays: this only waits for an end still on its way, which goes out with
            // the selected session's token; the task logged it if it's gone.
            let _ = local.stop().await;
        }
        for (bridge_url, token) in std::mem::take(&mut self.unheld) {
            // A session belongs to the Bridge that issued it: never send it to another.
            // `stored.bridge_url` is where calls go (`point_at`).
            if self.stored.bridge_url.as_ref() == Some(&bridge_url) {
                self.end_session(&token).await;
            } else {
                tracing::debug!("another Bridge: dropping a replaced session unsent");
            }
        }
    }

    /// `call` to the Bridge, as [`client::bridge_call`] makes it.
    async fn call<T>(
        &self,
        call: impl Future<Output = Result<T, ApiError>>,
    ) -> Result<T, ApiError> {
        client::bridge_call(self.call_timeout, call).await
    }

    /// `call` to the Bridge, as [`client::probe`] makes it.
    async fn probe<T>(
        &self,
        call: impl Future<Output = Result<T, ApiError>>,
    ) -> Result<T, ApiError> {
        client::probe(self.call_timeout, call).await
    }

    fn set_notice(&mut self, new: Notice) {
        if let Some(notice) = self.screen.notice_mut() {
            *notice = Some(new);
        }
    }
}

/// How much the Groups screen's volume buttons change a Group's volume.
const VOLUME_STEP: u8 = 5;

/// `volume` a [`VOLUME_STEP`] louder (`louder`) or quieter, within 0 to
/// [`api::MAX_VOLUME`].
pub fn volume_step(volume: u8, louder: bool) -> u8 {
    if louder {
        volume.saturating_add(VOLUME_STEP).min(api::MAX_VOLUME)
    } else {
        volume.saturating_sub(VOLUME_STEP)
    }
}

/// Shows `playback`, a control's answer, on `group`'s row: its state and position, and the
/// length if the answer tells one.
fn show_playback(group: &mut Group, playback: Playback) {
    group.state = Some(playback.state);
    if let Some(now_playing) = &mut group.now_playing {
        now_playing.position = playback.position;
        now_playing.duration = playback.duration.or(now_playing.duration);
    }
}

/// Lists episode `item` on `podcast` (if it is one of its episodes) as the podcast's filter
/// picks it by the Item's Progress: in its place where it was listed, at its place by date if
/// it newly passes, or left out. Among episodes with the same publish date (or none) a newly
/// listed one goes last, where the Bridge keeps the Library Server's order; cosmetic, and
/// listing the episodes afresh restores the Bridge's order.
fn relist_episode(podcast: &mut Podcast, item: &Item) {
    let Some(episode) = Episode::of(item) else {
        return;
    };
    if podcast.id != item.id.library_item {
        return;
    }
    let passes = podcast.filter.passes(episode.progress.as_ref());
    let listed = podcast
        .episodes
        .iter()
        .position(|listed| listed.id == episode.id);
    match (listed, passes) {
        (Some(at), true) => podcast.episodes[at] = episode,
        (Some(at), false) => {
            podcast.episodes.remove(at);
        }
        (None, true) => {
            let at = podcast
                .episodes
                .partition_point(|listed| listed.order() <= episode.order());
            podcast.episodes.insert(at, episode);
        }
        (None, false) => {}
    }
}

/// The Bridge base URL in `input`: `http` or `https` (assumed `http` without a scheme), a
/// host, maybe a port and path; no user name, password, query or fragment. Without a trailing
/// slash, since server function paths start with one.
fn bridge_url(input: &str) -> Option<String> {
    let input = input.trim();
    let url = if input.contains("://") {
        url::Url::parse(input)
    } else {
        url::Url::parse(&format!("http://{input}"))
    }
    .ok()?;
    let acceptable = matches!(url.scheme(), "http" | "https")
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none();
    acceptable.then(|| url.as_str().trim_end_matches('/').to_owned())
}

/// The path of [`api::library_servers`], which the App asks an entered address for.
const LIBRARY_SERVERS_PATH: &str = "/api/library-servers";

/// The Bridge base URL a request for the Library Servers was redirected to, `target`: the
/// same URL without that path. `None` if `target` asks for something else (a login page, a
/// query) or isn't a Bridge base URL then.
fn bridge_url_at(target: &url::Url) -> Option<String> {
    bridge_url(target.as_str().strip_suffix(LIBRARY_SERVERS_PATH)?)
}

/// Sends every server function call to the Bridge at `url`.
fn point_at(url: &str) {
    // The client keeps a `&'static str`. The App points at a new Bridge only when the person
    // enters one, so the leaked strings stay few and small.
    set_server_url(url.to_owned().leak());
}

/// Authenticates every server function call with the selected session's token, or with
/// nothing when none is selected.
fn authorize_selected(stored: &Stored) {
    authorize(stored.session().map(|session| &session.token));
}

/// Authenticates every server function call with `token`, or with nothing.
fn authorize(token: Option<&SessionToken>) {
    let mut headers = HeaderMap::new();
    if let Some(token) = token {
        match HeaderValue::from_str(&format!("Bearer {}", token.expose())) {
            Ok(mut value) => {
                value.set_sensitive(true);
                headers.insert(AUTHORIZATION, value);
            }
            // Calls then fail with `session_invalid`, which ends the session.
            Err(_) => tracing::warn!("the stored session token is not a valid header value"),
        }
    }
    set_request_headers(headers);
}

#[cfg(test)]
mod tests {
    use super::{bridge_url, bridge_url_at};

    #[test]
    fn bridge_urls_are_http_or_https_without_trailing_slash() {
        for (input, expected) in [
            ("http://192.168.1.10:8080", Some("http://192.168.1.10:8080")),
            (
                " https://bridge.example.com/ ",
                Some("https://bridge.example.com"),
            ),
            (
                "https://example.com/clacks/",
                Some("https://example.com/clacks"),
            ),
            ("192.168.1.10:8080", Some("http://192.168.1.10:8080")),
            ("bridge.local", Some("http://bridge.local")),
            ("ftp://bridge.local", None),
            ("http://user:pw@bridge.local", None),
            ("http://bridge.local/?x=1", None),
            ("http://bridge.local/#top", None),
            ("", None),
            ("http://", None),
        ] {
            assert_eq!(bridge_url(input).as_deref(), expected, "{input:?}");
        }
    }

    #[test]
    fn a_redirected_bridge_url_is_the_target_without_the_library_servers_path() {
        for (target, expected) in [
            (
                "https://bridge.example.com/api/library-servers",
                Some("https://bridge.example.com"),
            ),
            (
                "https://example.com:8443/clacks/api/library-servers",
                Some("https://example.com:8443/clacks"),
            ),
            (
                "http://127.0.0.1:9/api/library-servers",
                Some("http://127.0.0.1:9"),
            ),
            ("https://bridge.example.com/login", None),
            ("https://bridge.example.com/api/library-servers/", None),
            (
                "https://bridge.example.com/api/library-servers?next=1",
                None,
            ),
            (
                "https://user:pw@bridge.example.com/api/library-servers",
                None,
            ),
            ("ftp://bridge.example.com/api/library-servers", None),
        ] {
            let target = url::Url::parse(target).unwrap();
            assert_eq!(bridge_url_at(&target).as_deref(), expected, "{target}");
        }
    }
}
