//! The App's state and flow, apart from rendering: which screen shows, what it shows, and what
//! each action does, through the App API on the person's Bridge and the [`DeviceStore`].
//!
//! The UI renders [`App::screen`] and calls the actions; tests drive the same actions against
//! a real Bridge.
//!
//! Server functions reach the Bridge at a process-wide base URL and send a process-wide
//! `Authorization` header (Dioxus fullstack's client); [`App`] sets both, so one process runs
//! one [`App`] at a time.

use std::sync::Arc;
use std::time::Duration;

use api::{ApiError, ApiKey, LibraryServer, Session, SessionToken};
use dioxus::fullstack::http::header::{AUTHORIZATION, HeaderValue};
use dioxus::fullstack::{HeaderMap, get_server_url, set_request_headers, set_server_url};

use crate::i18n::{self, Language, LanguageChoice, Text};
use crate::storage::{DeviceStore, Stored, StoredSession};

/// What the App shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    /// Checking a stored session with the Bridge.
    Starting,
    /// Asks for the Bridge's address.
    BridgeUrl { notice: Option<Notice> },
    /// Picks a Library Server and takes an API key.
    Login {
        library_servers: Vec<LibraryServer>,
        notice: Option<Notice>,
    },
    /// Logged in: the Library Server, the account, the language, and logout.
    Settings { notice: Option<Notice> },
}

impl Screen {
    /// What the screen tells the person about the last action, if anything.
    pub fn notice(&self) -> Option<&Notice> {
        match self {
            Self::Starting => None,
            Self::BridgeUrl { notice } | Self::Login { notice, .. } | Self::Settings { notice } => {
                notice.as_ref()
            }
        }
    }

    fn notice_mut(&mut self) -> Option<&mut Option<Notice>> {
        match self {
            Self::Starting => None,
            Self::BridgeUrl { notice } | Self::Login { notice, .. } | Self::Settings { notice } => {
                Some(notice)
            }
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
    /// What the App saved on the device could not be read; it started afresh.
    StorageLoad,
    /// The device storage could not be written.
    StorageSave,
    /// Logged out, but the device storage still holds the session.
    LogoutNotSaved,
}

impl Notice {
    pub fn text(&self) -> Text {
        match self {
            Self::Api(error) => error.into(),
            Self::BridgeUrlInvalid => Text::ErrorBridgeUrlInvalid,
            Self::StorageLoad => Text::ErrorStorageLoad,
            Self::StorageSave => Text::ErrorStorageSave,
            Self::LogoutNotSaved => Text::ErrorLogoutNotSaved,
        }
    }
}

/// How long a call to the Bridge may take before the App treats the Bridge as unreachable.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// The App: what it remembers on the device and the screen it shows.
#[derive(Debug, Clone)]
pub struct App {
    store: Arc<dyn DeviceStore>,
    stored: Stored,
    device_language: Language,
    screen: Screen,
    /// Why the stored state could not be loaded, for the first screen.
    load_notice: Option<Notice>,
    call_timeout: Duration,
}

impl App {
    /// The App as it launches: what `store` remembers, on [`Screen::Starting`] until
    /// [`App::start`]. `device_locale` is the device's language (a BCP 47 tag such as `de-DE`).
    pub fn open(store: Arc<dyn DeviceStore>, device_locale: Option<&str>) -> Self {
        let (mut stored, load_notice) = match store.load() {
            Ok(stored) => (stored, None),
            Err(error) => {
                tracing::warn!(%error, "cannot load the App's stored state; starting afresh");
                (Stored::default(), Some(Notice::StorageLoad))
            }
        };
        // A session belongs to its Bridge; without one it can't be used.
        if stored.bridge_url.is_none() {
            stored.session = None;
        }
        if let Some(url) = &stored.bridge_url {
            point_at(url);
        }
        authorize(stored.session.as_ref().map(|session| &session.token));
        Self {
            store,
            stored,
            device_language: Language::for_locale(device_locale),
            screen: Screen::Starting,
            load_notice,
            call_timeout: CALL_TIMEOUT,
        }
    }

    /// This App with calls to the Bridge bounded by `timeout` instead of [`CALL_TIMEOUT`].
    pub fn with_call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// Picks the first screen: the Bridge address if there is none, Login if there is no
    /// session, else Settings once the Bridge confirmed the session. A session the Bridge
    /// ended (`api_key_rejected`, `session_invalid`) is forgotten; on any other failure (the
    /// Bridge or Library Server unreachable, for example) it stays.
    pub async fn start(&mut self) {
        let mut notice = self.load_notice.take();
        if self.stored.bridge_url.is_none() {
            self.screen = Screen::BridgeUrl { notice };
            return;
        }
        if self.stored.session.is_none() {
            self.show_login(notice).await;
            return;
        }
        match self.call(api::current_user()).await {
            Ok(user) => {
                if let Some(session) = &mut self.stored.session
                    && session.user != user
                {
                    session.user = user;
                    notice = notice.or(self.save());
                }
                self.screen = Screen::Settings { notice };
            }
            Err(error @ (ApiError::ApiKeyRejected | ApiError::SessionInvalid)) => {
                // The revocation matters more than a failed save, which `save` logged.
                self.forget_session();
                self.show_login(Some(Notice::Api(error))).await;
            }
            Err(error) => {
                self.screen = Screen::Settings {
                    notice: Some(Notice::Api(error)),
                };
            }
        }
    }

    /// Connects to the Bridge at `input` and remembers it if it answers with its Library
    /// Servers. Only while logged out: a session belongs to its Bridge.
    pub async fn submit_bridge_url(&mut self, input: &str) {
        if self.stored.session.is_some() {
            return;
        }
        let Some(url) = bridge_url(input) else {
            self.screen = Screen::BridgeUrl {
                notice: Some(Notice::BridgeUrlInvalid),
            };
            return;
        };
        let previous = get_server_url();
        point_at(&url);
        match self.call(api::library_servers()).await {
            Ok(library_servers) => {
                self.stored.bridge_url = Some(url);
                self.screen = Screen::Login {
                    library_servers,
                    notice: self.save(),
                };
            }
            Err(error) => {
                set_server_url(previous);
                self.screen = Screen::BridgeUrl {
                    notice: Some(Notice::Api(error)),
                };
            }
        }
    }

    /// Back to the Bridge address, to enter another one. Only while logged out.
    pub fn change_bridge_url(&mut self) {
        if self.stored.session.is_none() {
            self.screen = Screen::BridgeUrl { notice: None };
        }
    }

    /// Asks the Bridge for its Library Servers again, for Login.
    pub async fn reload_library_servers(&mut self) {
        if self.stored.session.is_none() && self.stored.bridge_url.is_some() {
            self.show_login(None).await;
        }
    }

    /// Logs in to the Library Server with base URL `library_server`. The key is sent to the
    /// Bridge and not kept.
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
                authorize(Some(&token));
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
                self.stored.session = Some(StoredSession {
                    token,
                    library_server,
                    user,
                });
                self.screen = Screen::Settings {
                    notice: self.save(),
                };
            }
            Err(error) => self.set_notice(Notice::Api(error)),
        }
    }

    /// Ends the session on the Bridge and forgets it on the device, also when the Bridge can't
    /// be reached.
    pub async fn log_out(&mut self) {
        if self.stored.session.is_none() {
            return;
        }
        if let Err(error) = self.call(api::logout()).await {
            tracing::info!(%error, "the Bridge did not end the session; forgetting it anyway");
        }
        let not_saved = self.forget_session().is_some();
        self.show_login(None).await;
        if not_saved {
            // The token is still on the device: the next launch may be logged in again.
            self.set_notice(Notice::LogoutNotSaved);
        }
    }

    /// Switches the language now and remembers the choice.
    pub fn set_language(&mut self, choice: LanguageChoice) {
        self.stored.language = choice;
        if let Some(notice) = self.save() {
            self.set_notice(notice);
        }
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// The remembered Bridge address.
    pub fn bridge_url(&self) -> Option<&str> {
        self.stored.bridge_url.as_deref()
    }

    /// The logged-in session: its Library Server and account.
    pub fn session(&self) -> Option<&StoredSession> {
        self.stored.session.as_ref()
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

    /// Forgets the session on the device. Returns a notice if that could not be saved.
    fn forget_session(&mut self) -> Option<Notice> {
        self.stored.session = None;
        authorize(None);
        self.save()
    }

    /// Saves what the App remembers. Returns a notice if that failed.
    fn save(&self) -> Option<Notice> {
        let error = self.store.save(&self.stored).err()?;
        tracing::warn!(%error, "cannot save the App's state on the device");
        Some(Notice::StorageSave)
    }

    /// `call` to the Bridge, or `bridge_unreachable` if it takes longer than the call timeout.
    async fn call<T>(
        &self,
        call: impl Future<Output = Result<T, ApiError>>,
    ) -> Result<T, ApiError> {
        tokio::time::timeout(self.call_timeout, call)
            .await
            .unwrap_or(Err(ApiError::BridgeUnreachable))
    }

    fn set_notice(&mut self, new: Notice) {
        if let Some(notice) = self.screen.notice_mut() {
            *notice = Some(new);
        }
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

/// Sends every server function call to the Bridge at `url`.
fn point_at(url: &str) {
    // The client keeps a `&'static str`. The App points at a new Bridge only when the person
    // enters one, so the leaked strings stay few and small.
    set_server_url(url.to_owned().leak());
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
    use super::bridge_url;

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
}
