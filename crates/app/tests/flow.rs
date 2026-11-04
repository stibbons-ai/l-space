//! The App's flow against a real Bridge and a real Audiobookshelf (PRD "Testing Decisions"):
//! each test drives [`App`] as the UI does and observes through it, the device storage it
//! leaves behind (by launching the App again on it), and the Bridge's App API.
//!
//! The App's server function client is process-wide, so tests take turns ([`run`]). The
//! Bridge binary must be built first (`cargo build -p bridge`; `mise run test` does).

use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use api::{ApiError, ApiKey, LibraryServer, Session};
use app::flow::{App, Notice, Screen};
use app::i18n::{self, Language, LanguageChoice, Text};
use app::storage::FileStore;
use tempfile::TempDir;
use test_support::audiobookshelf::{AbsUser, Audiobookshelf};
use test_support::bridge::{TestBridge, library_server_toml};
use test_support::closed_port;

/// One runtime for every test, as the App has one: the client's connection pool belongs to it.
static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("test runtime")
});

static TURN: Mutex<()> = Mutex::new(());

/// Runs `test` on the shared runtime while no other test runs.
fn run(test: impl AsyncFnOnce()) {
    let _turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
    RUNTIME.block_on(test());
}

/// The App's device storage, kept across launches.
struct Device {
    dir: TempDir,
    locale: &'static str,
}

impl Device {
    fn new() -> Self {
        Self::with_locale("en-US")
    }

    fn with_locale(locale: &'static str) -> Self {
        Self {
            dir: TempDir::new().expect("temp dir"),
            locale,
        }
    }

    /// Launches the App on this device and waits for its first screen.
    async fn launch(&self) -> App {
        let store = FileStore::new(self.dir.path().join("app.json"));
        let mut app = App::open(Arc::new(store), Some(self.locale));
        app.start().await;
        app
    }
}

/// A Bridge with the shared Audiobookshelf as its Library Server "Home", followed by one
/// nothing answers for.
fn bridge_with_abs() -> (TestBridge, Arc<Audiobookshelf>, String) {
    let abs = Audiobookshelf::shared();
    let unreachable = format!("http://{}", closed_port());
    let bridge = TestBridge::start(&format!(
        "{}name = \"Home\"\n{}",
        library_server_toml(abs.base_url()),
        library_server_toml(&unreachable),
    ));
    (bridge, abs, unreachable)
}

/// Launches the App on `device`, enters `bridge`'s URL and logs in as `user` on `abs`.
async fn logged_in(
    device: &Device,
    bridge: &TestBridge,
    abs: &Audiobookshelf,
    user: &AbsUser,
) -> App {
    let mut app = device.launch().await;
    app.submit_bridge_url(bridge.url()).await;
    app.log_in(abs.base_url(), ApiKey::new(&user.api_key.key))
        .await;
    assert_eq!(app.screen(), &Screen::Settings { notice: None });
    app
}

/// The session the App holds, to check it with the Bridge.
fn session(app: &App) -> Session {
    let stored = app.session().expect("logged in");
    Session {
        token: stored.token.clone(),
        user: stored.user.clone(),
    }
}

#[test]
fn the_bridge_url_is_entered_once_and_remembered() {
    let (bridge, abs, unreachable) = bridge_with_abs();
    let device = Device::new();
    run(async || {
        let mut app = device.launch().await;
        assert_eq!(app.screen(), &Screen::BridgeUrl { notice: None });

        app.submit_bridge_url("ftp://bridge.local").await;
        assert_eq!(
            app.screen(),
            &Screen::BridgeUrl {
                notice: Some(Notice::BridgeUrlInvalid)
            }
        );

        app.submit_bridge_url(&format!("http://{}", closed_port()))
            .await;
        assert_eq!(
            app.screen(),
            &Screen::BridgeUrl {
                notice: Some(Notice::Api(ApiError::BridgeUnreachable))
            }
        );
        // An address that didn't answer isn't remembered.
        assert_eq!(
            device.launch().await.screen(),
            &Screen::BridgeUrl { notice: None }
        );

        app.submit_bridge_url(&format!(" {}/ ", bridge.url())).await;
        let login = Screen::Login {
            library_servers: vec![
                LibraryServer {
                    base_url: abs.base_url().to_owned(),
                    name: Some("Home".to_owned()),
                },
                LibraryServer {
                    base_url: unreachable.clone(),
                    name: None,
                },
            ],
            notice: None,
        };
        assert_eq!(app.screen(), &login);

        let relaunched = device.launch().await;
        assert_eq!(relaunched.screen(), &login);
        assert_eq!(relaunched.bridge_url(), Some(bridge.url()));
    });
}

#[test]
fn login_with_an_api_key_opens_a_session_that_settings_shows() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async || {
        let user = abs.new_user().await;
        let app = logged_in(&device, &bridge, &abs, &user).await;

        let stored = app.session().expect("logged in");
        assert_eq!(
            stored.library_server,
            LibraryServer {
                base_url: abs.base_url().to_owned(),
                name: Some("Home".to_owned()),
            }
        );
        assert_eq!(stored.user.username, user.username);
        assert_eq!(app.bridge_url(), Some(bridge.url()));
        // The Bridge knows the session as this user's.
        assert_eq!(bridge.api().current_user(&session(&app)).await, stored.user);
    });
}

#[test]
fn a_rejected_key_or_unreachable_library_server_shows_a_translated_message() {
    let (bridge, abs, unreachable) = bridge_with_abs();
    let device = Device::with_locale("de-DE");
    run(async || {
        let user = abs.new_user().await;
        let mut app = device.launch().await;
        app.submit_bridge_url(bridge.url()).await;

        app.log_in(abs.base_url(), ApiKey::new("not-a-key")).await;
        assert_eq!(
            app.screen().notice(),
            Some(&Notice::Api(ApiError::ApiKeyRejected))
        );
        assert_eq!(app.session(), None);
        let message = app.text(Text::ErrorApiKeyRejected);
        assert_eq!(
            message,
            i18n::text(Language::German, Text::ErrorApiKeyRejected)
        );
        assert_ne!(
            message,
            i18n::text(Language::English, Text::ErrorApiKeyRejected)
        );

        app.log_in(&unreachable, ApiKey::new(&user.api_key.key))
            .await;
        assert_eq!(
            app.screen().notice(),
            Some(&Notice::Api(ApiError::LibraryServerUnreachable))
        );
        assert_eq!(app.session(), None);
    });
}

#[test]
fn a_session_lasts_across_launches_until_logout() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async || {
        let user = abs.new_user().await;
        let app = logged_in(&device, &bridge, &abs, &user).await;
        let before = session(&app);
        drop(app);

        let mut app = device.launch().await;
        assert_eq!(app.screen(), &Screen::Settings { notice: None });
        assert_eq!(session(&app), before);

        app.log_out().await;
        assert!(
            matches!(app.screen(), Screen::Login { notice: None, .. }),
            "{:?}",
            app.screen()
        );
        assert_eq!(app.session(), None);
        assert_eq!(
            bridge.api().current_user_error(&before).await.1,
            ApiError::SessionInvalid
        );

        let relaunched = device.launch().await;
        assert!(matches!(
            relaunched.screen(),
            Screen::Login { notice: None, .. }
        ));
        assert_eq!(relaunched.session(), None);
    });
}

#[test]
fn a_revoked_key_ends_the_session_at_the_next_launch_with_a_translated_message() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::with_locale("de-AT");
    run(async || {
        let user = abs.new_user().await;
        logged_in(&device, &bridge, &abs, &user).await;
        abs.delete_api_key(&user.api_key).await;

        let app = device.launch().await;
        assert!(
            matches!(app.screen(), Screen::Login { library_servers, notice: Some(Notice::Api(ApiError::ApiKeyRejected)) } if library_servers.len() == 2),
            "{:?}",
            app.screen()
        );
        assert_eq!(app.session(), None);
        assert_eq!(app.language(), Language::German);

        // Forgotten on the device, so the next launch asks for a key without a message.
        let relaunched = device.launch().await;
        assert!(matches!(
            relaunched.screen(),
            Screen::Login { notice: None, .. }
        ));
    });
}

#[test]
fn an_unreachable_bridge_keeps_the_session_at_launch() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async move || {
        let user = abs.new_user().await;
        let before = session(&logged_in(&device, &bridge, &abs, &user).await);
        drop(bridge);

        let app = device.launch().await;
        assert_eq!(
            app.screen(),
            &Screen::Settings {
                notice: Some(Notice::Api(ApiError::BridgeUnreachable))
            }
        );
        assert_eq!(app.session().map(|stored| &stored.user), Some(&before.user));
    });
}

#[test]
fn logout_forgets_the_session_even_when_the_bridge_is_unreachable() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async move || {
        let user = abs.new_user().await;
        let mut app = logged_in(&device, &bridge, &abs, &user).await;
        drop(bridge);

        app.log_out().await;
        assert_eq!(
            app.screen(),
            &Screen::Login {
                library_servers: Vec::new(),
                notice: Some(Notice::Api(ApiError::BridgeUnreachable)),
            }
        );
        assert_eq!(device.launch().await.session(), None);
    });
}

#[test]
fn the_language_follows_the_device_unless_settings_overrides_it() {
    run(async || {
        let german = Device::with_locale("de-DE");
        let mut app = german.launch().await;
        assert_eq!(app.language(), Language::German);
        assert_eq!(
            app.text(Text::BridgeUrlTitle),
            i18n::text(Language::German, Text::BridgeUrlTitle)
        );

        let other = Device::with_locale("fr-FR");
        assert_eq!(other.launch().await.language(), Language::English);

        app.set_language(LanguageChoice::English);
        assert_eq!(app.language(), Language::English);
        assert_eq!(
            app.text(Text::BridgeUrlTitle),
            i18n::text(Language::English, Text::BridgeUrlTitle)
        );
        let relaunched = german.launch().await;
        assert_eq!(relaunched.language_choice(), LanguageChoice::English);
        assert_eq!(relaunched.language(), Language::English);

        app.set_language(LanguageChoice::System);
        assert_eq!(german.launch().await.language(), Language::German);
    });
}
