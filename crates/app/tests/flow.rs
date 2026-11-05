//! The App's flow against a real Bridge and a real Audiobookshelf (PRD "Testing Decisions"):
//! each test drives [`App`] as the UI does and observes through it, the device storage it
//! leaves behind (by launching the App again on it), and the Bridge's App API.
//!
//! The App's server function client is process-wide, so tests take turns ([`run`]), and every
//! launch starts from a cold client ([`Device::launch`]). The Bridge binary must be built first
//! (`cargo build -p bridge`; `mise run test` does).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use api::{
    ApiError, ApiKey, EpisodeFilter, ItemId, Library, LibraryEntry, LibraryKind, LibraryServer,
    Progress, Session,
};
use app::flow::{App, Notice, Screen};
use app::i18n::{self, Language, LanguageChoice, Text};
use app::storage::{DeviceStore, FileStore, LoadError, Stored};
use tempfile::TempDir;
use test_support::audiobookshelf::{AbsUser, Audiobookshelf, EPISODE_PUBLISHED_AT};
use test_support::bridge::{TestBridge, library_server_toml, named_library_server_toml};
use test_support::closed_port;

/// The seeded audiobook Library.
fn audiobooks(abs: &Audiobookshelf) -> Library {
    Library {
        id: abs.seeded().audiobook_library.clone(),
        name: "Audiobooks".into(),
        kind: LibraryKind::Audiobooks,
    }
}

/// The seeded podcast Library.
fn podcasts(abs: &Audiobookshelf) -> Library {
    Library {
        id: abs.seeded().podcast_library.clone(),
        name: "Podcasts".into(),
        kind: LibraryKind::Podcasts,
    }
}

/// The screen listing every seeded Library, with `notice`: where a login lands.
fn libraries(abs: &Audiobookshelf, notice: Option<Notice>) -> Screen {
    Screen::Libraries {
        libraries: vec![audiobooks(abs), podcasts(abs)],
        notice,
    }
}

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

/// Short, so tests against something that never answers stay fast. Only those tests use it:
/// real calls through the Bridge to Audiobookshelf get the App's default.
const SHORT_CALL_TIMEOUT: Duration = Duration::from_millis(500);

/// The App's device storage, kept across launches. Saves fail while `fail_saves` is set.
#[derive(Debug, Clone)]
struct Device {
    dir: Arc<TempDir>,
    locale: &'static str,
    fail_saves: Arc<AtomicBool>,
}

impl Device {
    fn new() -> Self {
        Self::with_locale("en-US")
    }

    fn with_locale(locale: &'static str) -> Self {
        Self {
            dir: Arc::new(TempDir::new().expect("temp dir")),
            locale,
            fail_saves: Arc::default(),
        }
    }

    fn file(&self) -> FileStore {
        FileStore::new(self.path())
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("app.json")
    }

    /// The files in the device's App directory.
    fn files(&self) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(self.dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Launches the App on this device, as a fresh process would, and waits for its first
    /// screen.
    async fn launch(&self) -> App {
        self.launch_with(None).await
    }

    /// [`Device::launch`] with Bridge calls bounded by [`SHORT_CALL_TIMEOUT`].
    async fn launch_impatient(&self) -> App {
        self.launch_with(Some(SHORT_CALL_TIMEOUT)).await
    }

    async fn launch_with(&self, call_timeout: Option<Duration>) -> App {
        // A fresh process has no server URL and no `Authorization` header yet.
        dioxus::fullstack::set_server_url("");
        dioxus::fullstack::clear_request_headers();
        let mut app = App::open(Arc::new(self.clone()), Some(self.locale));
        if let Some(timeout) = call_timeout {
            app = app.with_call_timeout(timeout);
        }
        app.start().await;
        app
    }

    /// Changes what the device holds, as a previous App version or a disk error might.
    fn edit(&self, change: impl FnOnce(&mut serde_json::Value)) {
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(self.path()).expect("app.json")).unwrap();
        change(&mut json);
        std::fs::write(self.path(), json.to_string()).unwrap();
    }
}

impl DeviceStore for Device {
    fn load(&self) -> Result<Stored, LoadError> {
        self.file().load()
    }

    fn save(&self, stored: &Stored) -> io::Result<()> {
        if self.fail_saves.load(Ordering::SeqCst) {
            return Err(io::Error::other("disk full"));
        }
        self.file().save(stored)
    }
}

/// The configuration naming the shared Audiobookshelf `abs` as the Library Server "Home",
/// followed by `unreachable`.
fn home_config(abs: &Audiobookshelf, unreachable: &str) -> String {
    format!(
        "{}{}",
        named_library_server_toml(abs.base_url(), "Home"),
        library_server_toml(unreachable),
    )
}

/// A Bridge with the shared Audiobookshelf as its Library Server "Home", followed by one
/// nothing answers for.
fn bridge_with_abs() -> (TestBridge, Arc<Audiobookshelf>, String) {
    let abs = Audiobookshelf::shared();
    let unreachable = format!("http://{}", closed_port());
    let bridge = TestBridge::start(&home_config(&abs, &unreachable));
    (bridge, abs, unreachable)
}

/// Restarts `bridge` on `config` and points `device` at it: it may listen on another port.
fn restart_bridge(bridge: &mut TestBridge, device: &Device, config: &str) {
    bridge.restart_with(config);
    device.edit(|json| json["bridge_url"] = bridge.url().into());
}

/// Launches the App on `device`, enters `bridge`'s URL and logs in as `user` on `abs`.
async fn logged_in(
    device: &Device,
    bridge: &TestBridge,
    abs: &Audiobookshelf,
    user: &AbsUser,
) -> App {
    let app = logged_in_behind(device, bridge, abs.base_url(), user).await;
    assert_eq!(app.screen(), &libraries(abs, None));
    app
}

/// Launches the App on `device`, enters `bridge`'s URL and logs in as `user` on the Library
/// Server at `library_server`, whatever the login shows.
async fn logged_in_behind(
    device: &Device,
    bridge: &TestBridge,
    library_server: &str,
    user: &AbsUser,
) -> App {
    let mut app = device.launch().await;
    app.submit_bridge_url(bridge.url()).await;
    app.log_in(library_server, ApiKey::new(&user.api_key.key))
        .await;
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

/// The notice on screen, as the person reads it.
fn notice_text(app: &App) -> String {
    let notice = app.screen().notice().expect("a notice on screen");
    app.text(notice.text())
}

/// The screen's text in English and in German, by `read` from the App.
fn texts_of<T>(app: &mut App, read: impl Fn(&App) -> T) -> (T, T) {
    let choice = app.language_choice();
    app.set_language(LanguageChoice::English);
    let english = read(app);
    app.set_language(LanguageChoice::German);
    let german = read(app);
    app.set_language(choice);
    (english, german)
}

/// The notice on screen in English and in German.
fn notice_texts(app: &mut App) -> (String, String) {
    texts_of(app, notice_text)
}

fn both(text: Text) -> (String, String) {
    (
        i18n::text(Language::English, text),
        i18n::text(Language::German, text),
    )
}

/// A loopback server that accepts connections and never answers.
fn silent_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            held.push(stream);
        }
    });
    addr
}

/// A loopback web server that answers every request with `status` (e.g. `404 Not Found`) and
/// a `content_type` body.
fn web_server(status: &'static str, content_type: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut request = [0; 4096];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

/// A router's or captive portal's web page.
fn html_page_server() -> String {
    web_server("200 OK", "text/html", "<html><body>Welcome</body></html>")
}

/// What went through a [`bridge_then_gone`] proxy: each request line, and after the first the
/// status line the Bridge answered it with.
type ProxyLog = Arc<Mutex<Vec<String>>>;

/// A loopback proxy that passes the first request on to `bridge` and answers every later one
/// with `503 Service Unavailable`, as a reverse proxy does once the Bridge behind it is gone.
fn bridge_then_gone(bridge: &str) -> (String, ProxyLog) {
    let target = bridge
        .strip_prefix("http://")
        .expect("an http:// Bridge URL")
        .to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let log = ProxyLog::default();
    let written = Arc::clone(&log);
    std::thread::spawn(move || {
        for (index, mut stream) in listener.incoming().flatten().enumerate() {
            let request = read_request(&stream);
            let record = |bytes: &[u8]| {
                let line = String::from_utf8_lossy(bytes)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_owned();
                written.lock().unwrap().push(line);
            };
            record(&request);
            let response = if index == 0 {
                let response = forward(&target, &request);
                record(&response);
                response
            } else {
                UNAVAILABLE.to_vec()
            };
            let _ = stream.write_all(&response);
        }
    });
    (format!("http://{addr}"), log)
}

/// One HTTP/1.1 request from `stream`, rewritten to close the connection after the response,
/// so the next request comes on a new one.
fn read_request(stream: &TcpStream) -> Vec<u8> {
    let mut reader = BufReader::new(stream);
    let mut head = String::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
        if !lower.starts_with("connection:") {
            head.push_str(&line);
        }
    }
    head.push_str("connection: close\r\n\r\n");
    let mut request = head.into_bytes();
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    request.extend(body);
    request
}

/// What a reverse proxy answers when what is behind it is gone.
const UNAVAILABLE: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

/// Sends `request` (which closes its connection) to `target` (`host:port`) and returns the
/// whole response.
fn forward(target: &str, request: &[u8]) -> Vec<u8> {
    let mut server = TcpStream::connect(target).unwrap();
    server.write_all(request).unwrap();
    let mut response = Vec::new();
    server.read_to_end(&mut response).unwrap();
    response
}

/// Which requests an [`abs_proxy`] fails: those whose request line starts with this.
type Failing = Arc<Mutex<Option<&'static str>>>;

/// A loopback proxy in front of Audiobookshelf at `abs` (`host:port`) that answers the
/// requests `failing` names with `503 Service Unavailable` and passes every other one on.
fn abs_proxy(abs: &str) -> (String, Failing) {
    let target = abs.to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let failing = Failing::default();
    let fails = Arc::clone(&failing);
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let (target, fails) = (target.clone(), Arc::clone(&fails));
            std::thread::spawn(move || {
                let request = read_request(&stream);
                let fail = fails
                    .lock()
                    .unwrap()
                    .is_some_and(|prefix| request.starts_with(prefix.as_bytes()));
                let response = if fail {
                    UNAVAILABLE.to_vec()
                } else {
                    forward(&target, &request)
                };
                let _ = stream.write_all(&response);
            });
        }
    });
    (format!("http://{addr}"), failing)
}

/// A Bridge whose Library Server "Home" is the shared Audiobookshelf behind an [`abs_proxy`].
fn bridge_behind_proxy() -> (TestBridge, Arc<Audiobookshelf>, String, Failing) {
    let abs = Audiobookshelf::shared();
    let (proxy, failing) = abs_proxy(abs.address());
    let bridge = TestBridge::start(&named_library_server_toml(&proxy, "Home"));
    (bridge, abs, proxy, failing)
}

/// Fails the test instead of hanging when `action` outlives `limit`.
async fn within<T>(limit: Duration, action: impl Future<Output = T>) -> T {
    tokio::time::timeout(limit, action)
        .await
        .unwrap_or_else(|_| panic!("still waiting after {limit:?}: the call timeout didn't apply"))
}

#[test]
fn the_bridge_url_is_entered_once_and_remembered() {
    let (bridge, abs, unreachable) = bridge_with_abs();
    let device = Device::new();
    run(async || {
        let mut app = device.launch().await;
        assert_eq!(app.screen(), &Screen::BridgeUrl { notice: None });

        app.submit_bridge_url("ftp://bridge.local").await;
        assert_eq!(app.screen().notice(), Some(&Notice::BridgeUrlInvalid));

        app.submit_bridge_url(&format!("http://{}", closed_port()))
            .await;
        assert_eq!(
            app.screen(),
            &Screen::BridgeUrl {
                notice: Some(Notice::BridgeUrlUnreachable)
            }
        );
        // Here the address can be changed, so the message says to check it.
        assert_eq!(
            notice_texts(&mut app),
            both(Text::ErrorBridgeUrlUnreachable)
        );
        // An address that didn't answer isn't remembered.
        assert_eq!(
            device.launch().await.screen(),
            &Screen::BridgeUrl { notice: None }
        );

        // Without a scheme, as people type it: the App assumes `http://`.
        let typed = bridge.url().strip_prefix("http://").unwrap();
        app.submit_bridge_url(&format!(" {typed}/ ")).await;
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
fn an_address_that_is_not_a_bridge_is_named_as_such_and_not_remembered() {
    let abs = Audiobookshelf::shared();
    let not_found = web_server("404 Not Found", "text/plain", "Not Found");
    let html = html_page_server();
    let device = Device::new();
    run(async || {
        let mut app = device.launch().await;
        for address in [abs.base_url(), &not_found, &html] {
            app.submit_bridge_url(address).await;
            assert_eq!(
                app.screen(),
                &Screen::BridgeUrl {
                    notice: Some(Notice::Api(ApiError::NotABridge))
                },
                "{address}"
            );
            assert_eq!(notice_texts(&mut app), both(Text::ErrorNotABridge));
            assert_eq!(device.launch().await.bridge_url(), None, "{address}");
        }

        // A proxy whose Bridge is down.
        for status in [
            "502 Bad Gateway",
            "503 Service Unavailable",
            "504 Gateway Timeout",
        ] {
            let proxy = web_server(status, "text/html", "<h1>proxy</h1>");
            app.submit_bridge_url(&proxy).await;
            assert_eq!(
                app.screen().notice(),
                Some(&Notice::BridgeUrlUnreachable),
                "{status}"
            );
        }
    });
}

#[test]
fn login_with_an_api_key_opens_a_session_and_lists_the_libraries() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async || {
        let user = abs.new_user().await;
        let mut app = device.launch().await;
        app.submit_bridge_url(bridge.url()).await;
        // As pasted, with a line break.
        app.log_in(
            abs.base_url(),
            ApiKey::new(format!(" {}\n", user.api_key.key)),
        )
        .await;
        assert_eq!(app.screen(), &libraries(&abs, None));

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

        // The device keeps the session, readable by its owner only, and not the key.
        let file = std::fs::read_to_string(device.path()).unwrap();
        assert!(!file.contains(&user.api_key.key));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(device.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
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
        assert_eq!(notice_texts(&mut app), both(Text::ErrorApiKeyRejected));

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
        assert_eq!(app.screen(), &libraries(&abs, None));
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

        let mut app = device.launch().await;
        assert!(
            matches!(app.screen(), Screen::Login { library_servers, notice: Some(Notice::Api(ApiError::ApiKeyRejected)) } if library_servers.len() == 2),
            "{:?}",
            app.screen()
        );
        assert_eq!(app.session(), None);
        assert_eq!(app.language(), Language::German);
        assert_eq!(notice_texts(&mut app), both(Text::ErrorApiKeyRejected));

        // Forgotten on the device, so the next launch asks for a key without a message.
        let relaunched = device.launch().await;
        assert!(matches!(
            relaunched.screen(),
            Screen::Login { notice: None, .. }
        ));
    });
}

#[test]
fn a_session_the_bridge_ended_is_forgotten_at_the_next_launch() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async || {
        let user = abs.new_user().await;
        let before = session(&logged_in(&device, &bridge, &abs, &user).await);
        // Ended elsewhere, through the Bridge.
        bridge.api().logout(&before).await;

        let mut app = device.launch().await;
        assert_eq!(
            app.screen().notice(),
            Some(&Notice::Api(ApiError::SessionInvalid))
        );
        // Login with the list the App fetched to tell this apart from an unconfigured server.
        assert!(
            matches!(app.screen(), Screen::Login { library_servers, .. } if library_servers.len() == 2),
            "{:?}",
            app.screen()
        );
        assert_eq!(app.session(), None);
        assert_eq!(notice_texts(&mut app), both(Text::ErrorSessionInvalid));

        let relaunched = device.launch().await;
        assert!(matches!(
            relaunched.screen(),
            Screen::Login { notice: None, .. }
        ));
        assert_eq!(relaunched.session(), None);
    });
}

#[test]
fn a_session_whose_library_server_is_unconfigured_for_a_while_stays() {
    let (mut bridge, abs, unreachable) = bridge_with_abs();
    let device = Device::with_locale("de-DE");
    run(async move || {
        let user = abs.new_user().await;
        let before = session(&logged_in(&device, &bridge, &abs, &user).await);

        // The operator takes the Library Server out of the configuration for a while.
        restart_bridge(&mut bridge, &device, &library_server_toml(&unreachable));
        let mut app = device.launch().await;
        assert_eq!(
            app.screen(),
            &Screen::Settings {
                notice: Some(Notice::SessionLibraryServerNotConfigured)
            }
        );
        assert_eq!(session(&app), before);
        assert_eq!(
            notice_texts(&mut app),
            both(Text::ErrorSessionLibraryServerNotConfigured)
        );

        // Back in the configuration: the session kept on the device works again.
        restart_bridge(&mut bridge, &device, &home_config(&abs, &unreachable));
        let app = device.launch().await;
        assert_eq!(app.screen(), &libraries(&abs, None));
        assert_eq!(session(&app), before);
        assert_eq!(bridge.api().current_user(&before).await, before.user);
    });
}

#[test]
fn a_failed_library_server_listing_after_session_invalid_keeps_the_session() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async || {
        let user = abs.new_user().await;
        let before = session(&logged_in(&device, &bridge, &abs, &user).await);
        bridge.api().logout(&before).await;
        // The Bridge answers `session_invalid`, then can't be reached for the listing that
        // would tell an ended session from an unconfigured Library Server.
        let (proxy, log) = bridge_then_gone(bridge.url());
        device.edit(|json| json["bridge_url"] = proxy.clone().into());

        let app = device.launch().await;
        assert_eq!(
            app.screen(),
            &Screen::Settings {
                notice: Some(Notice::Api(ApiError::BridgeUnreachable))
            }
        );
        assert_eq!(session(&app), before);
        // The Bridge itself answered `session_invalid` (401), and the App then asked for the
        // list: not a failed first call that keeps the session anyway.
        let seen = log.lock().unwrap().clone();
        assert!(
            matches!(&seen[..], [current_user, answer, listing]
                if current_user.starts_with("GET /api/me ")
                    && answer.starts_with("HTTP/1.1 401 ")
                    && listing.starts_with("GET /api/library-servers ")),
            "{seen:?}"
        );
        // Kept on the device: the next launch, with the Bridge gone, still holds it.
        assert_eq!(session(&device.launch().await), before);
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
fn a_bridge_that_never_answers_counts_as_unreachable() {
    let (bridge, abs, _) = bridge_with_abs();
    let silent = format!("http://{}", silent_server());
    let device = Device::new();
    let limit = SHORT_CALL_TIMEOUT * 10;
    run(async || {
        let user = abs.new_user().await;
        let before = session(&logged_in(&device, &bridge, &abs, &user).await);
        // The Bridge's address now leads to something that accepts and never answers.
        device.edit(|json| json["bridge_url"] = silent.clone().into());

        let mut app = within(limit, device.launch_impatient()).await;
        assert_eq!(
            app.screen(),
            &Screen::Settings {
                notice: Some(Notice::Api(ApiError::BridgeUnreachable))
            }
        );
        assert_eq!(app.session().map(|stored| &stored.user), Some(&before.user));

        within(limit, app.log_out()).await;
        assert_eq!(app.session(), None);
        let relaunched = within(limit, device.launch_impatient()).await;
        assert_eq!(relaunched.session(), None);

        let fresh = Device::new();
        let mut app = fresh.launch_impatient().await;
        within(limit, app.submit_bridge_url(&silent)).await;
        assert_eq!(app.screen().notice(), Some(&Notice::BridgeUrlUnreachable));
        assert_eq!(fresh.launch().await.bridge_url(), None);

        // The App's default bound applies too (15 s; `within` fails long after).
        let mut app = fresh.launch().await;
        within(Duration::from_secs(25), app.submit_bridge_url(&silent)).await;
        assert_eq!(app.screen().notice(), Some(&Notice::BridgeUrlUnreachable));
    });
}

#[test]
fn a_slow_library_server_is_named_and_not_mistaken_for_an_unreachable_bridge() {
    // The Bridge gives up on a Library Server after 10 seconds; the App waits longer.
    let silent = format!("http://{}", silent_server());
    let bridge = TestBridge::start(&named_library_server_toml(&silent, "Slow"));
    let device = Device::new();
    run(async || {
        let mut app = device.launch().await;
        app.submit_bridge_url(bridge.url()).await;
        app.log_in(&silent, ApiKey::new("any-key")).await;
        assert_eq!(
            app.screen().notice(),
            Some(&Notice::Api(ApiError::LibraryServerUnreachable))
        );
    });
}

#[test]
fn a_captive_portal_at_the_stored_address_keeps_the_session() {
    let (bridge, abs, _) = bridge_with_abs();
    let html = html_page_server();
    let device = Device::new();
    run(async || {
        let user = abs.new_user().await;
        let before = session(&logged_in(&device, &bridge, &abs, &user).await);
        device.edit(|json| json["bridge_url"] = html.clone().into());

        let mut app = device.launch().await;
        assert_eq!(
            app.screen(),
            &Screen::Settings {
                notice: Some(Notice::Api(ApiError::BridgeUnreachable))
            }
        );
        assert_eq!(app.session().map(|stored| &stored.user), Some(&before.user));
        // Settings can't change the address, so the message doesn't say to check it.
        assert_eq!(notice_texts(&mut app), both(Text::ErrorBridgeUnreachable));
    });
}

#[test]
fn the_account_is_refreshed_at_launch() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async move || {
        let user = abs.new_user().await;
        logged_in(&device, &bridge, &abs, &user).await;
        let renamed = format!("{}-renamed", user.username);
        abs.rename_user(&user.id, &renamed).await;

        let app = device.launch().await;
        assert_eq!(app.screen(), &libraries(&abs, None));
        assert_eq!(app.session().unwrap().user.username, renamed);

        // Remembered: shown even when the Bridge can't confirm it.
        drop(bridge);
        let app = device.launch_impatient().await;
        assert_eq!(app.session().unwrap().user.username, renamed);
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
fn failed_saves_are_reported_until_a_save_succeeds() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async move || {
        let user = abs.new_user().await;

        // Entering the address.
        device.fail_saves.store(true, Ordering::SeqCst);
        let mut app = device.launch().await;
        app.submit_bridge_url(bridge.url()).await;
        assert!(matches!(
            app.screen(),
            Screen::Login {
                notice: Some(Notice::StorageSave),
                ..
            }
        ));
        device.fail_saves.store(false, Ordering::SeqCst);
        assert_eq!(device.launch().await.bridge_url(), None);

        // Logging in.
        let mut app = device.launch().await;
        app.submit_bridge_url(bridge.url()).await;
        device.fail_saves.store(true, Ordering::SeqCst);
        app.log_in(abs.base_url(), ApiKey::new(&user.api_key.key))
            .await;
        assert_eq!(app.screen(), &libraries(&abs, Some(Notice::StorageSave)));
        assert_eq!(app.text(Text::ErrorStorageSave), notice_text(&app));

        // Picking a language: a working save clears the notice; a failing one, from a clean
        // screen, shows it again.
        device.fail_saves.store(false, Ordering::SeqCst);
        app.set_language(LanguageChoice::English);
        assert_eq!(app.screen().notice(), None);
        device.fail_saves.store(true, Ordering::SeqCst);
        app.set_language(LanguageChoice::German);
        assert_eq!(app.screen().notice(), Some(&Notice::StorageSave));
        assert_eq!(app.language(), Language::German);
        device.fail_saves.store(false, Ordering::SeqCst);
        app.set_language(LanguageChoice::English);
        assert_eq!(app.screen().notice(), None);

        // Logging out with the Bridge gone and the device failing.
        drop(bridge);
        device.fail_saves.store(true, Ordering::SeqCst);
        app.log_out().await;
        assert_eq!(app.session(), None);
        assert_eq!(app.screen().notice(), Some(&Notice::LogoutNotSaved));
        assert_eq!(
            notice_text(&app),
            i18n::text(Language::English, Text::ErrorLogoutNotSaved)
        );
        // Saving the logged-out state later clears it.
        device.fail_saves.store(false, Ordering::SeqCst);
        app.set_language(LanguageChoice::German);
        assert_eq!(app.screen().notice(), None);
        assert_eq!(device.launch_impatient().await.session(), None);
    });
}

#[test]
fn a_revocation_at_launch_that_cannot_be_saved_is_found_again() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async || {
        let user = abs.new_user().await;
        logged_in(&device, &bridge, &abs, &user).await;
        abs.delete_api_key(&user.api_key).await;

        device.fail_saves.store(true, Ordering::SeqCst);
        let app = device.launch().await;
        assert_eq!(
            app.screen().notice(),
            Some(&Notice::Api(ApiError::ApiKeyRejected))
        );
        assert_eq!(app.session(), None);

        // Still on the device, so the next launch checks it again; the Bridge ended it.
        device.fail_saves.store(false, Ordering::SeqCst);
        let app = device.launch().await;
        assert_eq!(
            app.screen().notice(),
            Some(&Notice::Api(ApiError::SessionInvalid))
        );
        assert_eq!(device.launch().await.screen().notice(), None);
    });
}

#[test]
fn an_unreadable_device_file_is_set_aside_and_reported() {
    let device = Device::new();
    run(async || {
        // An earlier unreadable file stays.
        std::fs::write(device.dir.path().join("app.json.bad"), b"earlier").unwrap();
        std::fs::write(device.path(), b"{ not json").unwrap();

        let mut app = device.launch().await;
        assert_eq!(
            app.screen(),
            &Screen::BridgeUrl {
                notice: Some(Notice::StorageLoad)
            }
        );
        assert_eq!(notice_texts(&mut app), both(Text::ErrorStorageLoad));
        let dir = device.dir.path();
        assert_eq!(std::fs::read(dir.join("app.json.bad")).unwrap(), b"earlier");
        assert_eq!(
            std::fs::read(dir.join("app.json.bad.2")).unwrap(),
            b"{ not json"
        );

        // A file the App may not even read is set aside too.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::remove_file(device.path()).unwrap();
            std::fs::write(device.path(), b"{}").unwrap();
            std::fs::set_permissions(device.path(), std::fs::Permissions::from_mode(0o000))
                .unwrap();
            // Root reads it anyway; then there's nothing to test here.
            if std::fs::read(device.path()).is_err() {
                let app = device.launch().await;
                assert_eq!(app.screen().notice(), Some(&Notice::StorageLoad));
                assert!(dir.join("app.json.bad.3").exists());
            } else {
                std::fs::remove_file(device.path()).unwrap();
            }
        }

        // Every set-aside above removed the original.
        assert!(!device.path().exists());
        assert_eq!(
            device.launch().await.screen(),
            &Screen::BridgeUrl { notice: None }
        );
    });
}

#[cfg(unix)]
#[test]
fn an_unreadable_device_file_that_cannot_be_set_aside_is_left_alone() {
    use std::os::unix::fs::PermissionsExt;
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async || {
        let user = abs.new_user().await;
        std::fs::write(device.path(), b"{ not json").unwrap();
        let dir = device.dir.path();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root writes there anyway; then there's nothing to test.
        let probe = dir.join("probe");
        if std::fs::write(&probe, b"").is_ok() {
            std::fs::remove_file(&probe).unwrap();
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipped: running with privileges that ignore directory permissions");
            return;
        }

        let mut app = device.launch().await;
        assert_eq!(
            app.screen(),
            &Screen::BridgeUrl {
                notice: Some(Notice::StorageStuck)
            }
        );
        assert_eq!(notice_texts(&mut app), both(Text::ErrorStorageStuck));
        // Usable for this launch, without touching the file, and every change says it won't
        // be kept.
        app.submit_bridge_url(bridge.url()).await;
        assert!(matches!(
            app.screen(),
            Screen::Login {
                notice: Some(Notice::StorageStuck),
                ..
            }
        ));
        // Logging in and out: nothing of this launch was written, so logging out doesn't
        // claim the device still holds a session.
        app.log_in(abs.base_url(), ApiKey::new(&user.api_key.key))
            .await;
        assert_eq!(app.screen(), &libraries(&abs, Some(Notice::StorageStuck)));
        app.log_out().await;
        assert_eq!(app.screen().notice(), Some(&Notice::StorageStuck));
        assert_eq!(notice_texts(&mut app), both(Text::ErrorStorageStuck));
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(std::fs::read(device.path()).unwrap(), b"{ not json");
        assert_eq!(device.files(), ["app.json"]);
    });
}

#[test]
fn an_unknown_language_or_a_session_without_a_bridge_starts_cleanly() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async || {
        let user = abs.new_user().await;
        let before = session(&logged_in(&device, &bridge, &abs, &user).await);

        // A language from a newer App: the session and address stay.
        device.edit(|json| json["language"] = "klingon".into());
        let app = device.launch().await;
        assert_eq!(app.screen(), &libraries(&abs, None));
        assert_eq!(app.language_choice(), LanguageChoice::System);
        assert_eq!(app.session().map(|stored| &stored.user), Some(&before.user));

        // A session without the Bridge it belongs to can't be used.
        device.edit(|json| json["bridge_url"] = serde_json::Value::Null);
        let app = device.launch().await;
        assert_eq!(app.screen(), &Screen::BridgeUrl { notice: None });
        assert_eq!(app.session(), None);
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

#[test]
fn a_podcasts_episodes_are_filtered_by_the_persons_progress() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async || {
        let seeded = abs.seeded();
        let user = abs.new_user().await;
        abs.set_progress(&user, &seeded.podcast, Some(&seeded.episodes[0]), 0.0, true)
            .await;
        abs.set_progress(
            &user,
            &seeded.podcast,
            Some(&seeded.episodes[1]),
            300.0,
            false,
        )
        .await;
        let mut app = logged_in(&device, &bridge, &abs, &user).await;
        assert!(!app.can_go_back());

        app.open_library(podcasts(&abs)).await;
        let library = Screen::Library {
            library: podcasts(&abs),
            entries: vec![LibraryEntry {
                id: seeded.podcast.clone(),
                title: "The Clacks Hour".into(),
                author: Some("Ankh-Morpork Radio".into()),
            }],
            notice: None,
        };
        assert_eq!(app.screen(), &library);

        let episodes = |app: &App| match app.screen() {
            Screen::Podcast {
                podcast, filter, ..
            } => (
                *filter,
                podcast
                    .episodes
                    .iter()
                    .map(|episode| episode.title.clone())
                    .collect::<Vec<_>>(),
            ),
            screen => panic!("{screen:?}"),
        };
        app.open_entry(&seeded.podcast).await;
        assert_eq!(
            episodes(&app),
            (
                EpisodeFilter::Unfinished,
                vec!["Episode 3".into(), "Episode 2".into()]
            )
        );
        app.filter_episodes(EpisodeFilter::Finished).await;
        assert_eq!(
            episodes(&app),
            (EpisodeFilter::Finished, vec!["Episode 1".into()])
        );
        app.filter_episodes(EpisodeFilter::All).await;
        assert_eq!(
            episodes(&app),
            (
                EpisodeFilter::All,
                vec!["Episode 3".into(), "Episode 2".into(), "Episode 1".into()]
            )
        );
        // What each episode's row says, in both languages.
        let rows = |app: &App| match app.screen() {
            Screen::Podcast { podcast, .. } => podcast
                .episodes
                .iter()
                .map(|episode| i18n::episode_summary(app.language(), episode))
                .collect::<Vec<_>>(),
            screen => panic!("{screen:?}"),
        };
        assert_eq!(
            texts_of(&mut app, rows),
            (
                vec![
                    "March 31, 2026 · Not started".to_owned(),
                    "February 15, 2026 · 10 min left".to_owned(),
                    "January 1, 2026 · Finished".to_owned(),
                ],
                vec![
                    "31. März 2026 · Nicht begonnen".to_owned(),
                    "15. Februar 2026 · noch 10 Min.".to_owned(),
                    "1. Januar 2026 · Beendet".to_owned(),
                ],
            )
        );

        // Back the way it came.
        app.go_back();
        assert_eq!(app.screen(), &library);
        app.go_back();
        assert_eq!(app.screen(), &libraries(&abs, None));
        assert!(!app.can_go_back());
    });
}

#[test]
fn an_audiobook_shows_its_details_progress_and_chapters_in_english_and_german() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async || {
        let seeded = abs.seeded();
        let user = abs.new_user().await;
        abs.set_progress(&user, &seeded.long_book, None, 600.0, false)
            .await;
        let mut app = logged_in(&device, &bridge, &abs, &user).await;

        app.open_library(audiobooks(&abs)).await;
        let Screen::Library { entries, .. } = app.screen() else {
            panic!("{:?}", app.screen());
        };
        let titles: Vec<_> = entries.iter().map(|entry| entry.title.as_str()).collect();
        assert_eq!(titles, ["Silence", "The Long Book"]);

        app.open_entry(&seeded.long_book).await;
        let Screen::Item { item, notice: None } = app.screen() else {
            panic!("{:?}", app.screen());
        };
        assert_eq!(
            (
                item.title.as_str(),
                item.author.as_deref(),
                item.description.as_deref()
            ),
            (
                "The Long Book",
                Some("Pratchett"),
                Some("A book that goes on.")
            )
        );
        assert_eq!(
            item.progress,
            Some(Progress {
                current_time: 600.0,
                finished: false
            })
        );
        let chapters: Vec<_> = item
            .chapters
            .iter()
            .map(|chapter| (chapter.title.as_str(), i18n::clock(chapter.start)))
            .collect();
        assert_eq!(
            chapters,
            [
                ("01 - Part One", "0:00".to_owned()),
                ("02 - Part Two", "40:00".to_owned()),
                ("03 - Part Three", "1:10:00".to_owned()),
            ]
        );
        let item = item.clone();
        let facts = |app: &App| i18n::item_facts(app.language(), &item);
        assert_eq!(
            texts_of(&mut app, facts),
            (
                vec![
                    (Text::ItemProgress, "1 h 2 min left".to_owned()),
                    (Text::ItemDuration, "1 h 12 min".to_owned()),
                ],
                vec![
                    (Text::ItemProgress, "noch 1 Std. 2 Min.".to_owned()),
                    (Text::ItemDuration, "1 Std. 12 Min.".to_owned()),
                ],
            )
        );

        app.go_back();
        app.go_back();
        assert_eq!(app.screen(), &libraries(&abs, None));
    });
}

#[test]
fn an_episode_shows_its_podcast_publish_date_and_progress() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::with_locale("de-DE");
    run(async || {
        let seeded = abs.seeded();
        let user = abs.new_user().await;
        let mut app = logged_in(&device, &bridge, &abs, &user).await;
        app.open_library(podcasts(&abs)).await;
        app.open_podcast(&seeded.podcast).await;

        app.open_episode(&seeded.episodes[2]).await;
        let Screen::Item { item, notice: None } = app.screen() else {
            panic!("{:?}", app.screen());
        };
        assert_eq!(
            (
                item.title.as_str(),
                item.podcast.as_deref(),
                item.published_at
            ),
            (
                "Episode 3",
                Some("The Clacks Hour"),
                Some(EPISODE_PUBLISHED_AT[2])
            )
        );
        assert!(item.chapters.is_empty());
        let item = item.clone();
        let facts = |app: &App| i18n::item_facts(app.language(), &item);
        assert_eq!(
            texts_of(&mut app, facts),
            (
                vec![
                    (Text::ItemProgress, "Not started".to_owned()),
                    (Text::ItemDuration, "1 h 6 min".to_owned()),
                    (Text::ItemPublished, "March 31, 2026".to_owned()),
                ],
                vec![
                    (Text::ItemProgress, "Nicht begonnen".to_owned()),
                    (Text::ItemDuration, "1 Std. 6 Min.".to_owned()),
                    (Text::ItemPublished, "31. März 2026".to_owned()),
                ],
            )
        );

        // Back to the episodes, as they were.
        app.go_back();
        assert!(
            matches!(app.screen(), Screen::Podcast { filter: EpisodeFilter::Unfinished, podcast, .. } if podcast.episodes.len() == 3),
            "{:?}",
            app.screen()
        );
    });
}

#[test]
fn settings_stay_reachable_from_the_libraries() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async || {
        let user = abs.new_user().await;
        let mut app = logged_in(&device, &bridge, &abs, &user).await;

        app.open_settings();
        assert_eq!(app.screen(), &Screen::Settings { notice: None });
        assert!(!app.can_go_back());

        app.open_libraries().await;
        assert_eq!(app.screen(), &libraries(&abs, None));
    });
}

#[test]
fn something_no_longer_on_the_library_server_is_named_on_the_screen_shown() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async || {
        let user = abs.new_user().await;
        let mut app = logged_in(&device, &bridge, &abs, &user).await;

        app.open_item(ItemId {
            library_item: "00000000-0000-0000-0000-000000000000".into(),
            episode: None,
        })
        .await;
        assert_eq!(
            app.screen(),
            &libraries(&abs, Some(Notice::Api(ApiError::NotFound)))
        );
        assert_eq!(notice_texts(&mut app), both(Text::ErrorNotFound));
        assert!(app.session().is_some());
    });
}

#[test]
fn a_key_revoked_while_browsing_ends_the_session() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async || {
        let user = abs.new_user().await;
        let mut app = logged_in(&device, &bridge, &abs, &user).await;
        abs.delete_api_key(&user.api_key).await;

        app.open_library(audiobooks(&abs)).await;
        assert!(
            matches!(
                app.screen(),
                Screen::Login {
                    notice: Some(Notice::Api(ApiError::ApiKeyRejected)),
                    ..
                }
            ),
            "{:?}",
            app.screen()
        );
        assert_eq!(app.session(), None);
        assert!(!app.can_go_back());
        assert_eq!(device.launch().await.session(), None);
    });
}

#[test]
fn libraries_that_cannot_be_listed_are_explained_and_listed_on_retry() {
    let (bridge, abs, proxy, failing) = bridge_behind_proxy();
    let device = Device::new();
    run(async || {
        let user = abs.new_user().await;
        *failing.lock().unwrap() = Some("GET /api/libraries ");
        let failed = Screen::Libraries {
            libraries: Vec::new(),
            notice: Some(Notice::Api(ApiError::LibraryServerUnreachable)),
        };

        // After login...
        let mut app = logged_in_behind(&device, &bridge, &proxy, &user).await;
        assert_eq!(app.screen(), &failed);
        assert_eq!(
            notice_texts(&mut app),
            both(Text::ErrorLibraryServerUnreachable)
        );
        assert!(app.session().is_some());

        // ...and at launch, the session staying.
        let mut app = device.launch().await;
        assert_eq!(app.screen(), &failed);

        *failing.lock().unwrap() = None;
        app.open_libraries().await;
        assert_eq!(app.screen(), &libraries(&abs, None));
    });
}

#[test]
fn a_session_ended_elsewhere_while_browsing_leads_to_login() {
    let (bridge, abs, _) = bridge_with_abs();
    let device = Device::new();
    run(async || {
        let seeded = abs.seeded();
        let user = abs.new_user().await;
        let mut app = logged_in(&device, &bridge, &abs, &user).await;
        app.open_library(podcasts(&abs)).await;
        app.open_entry(&seeded.podcast).await;
        assert!(matches!(app.screen(), Screen::Podcast { .. }));
        // Logged out on the Bridge, as from another device holding the session.
        bridge.api().logout(&session(&app)).await;

        app.filter_episodes(EpisodeFilter::All).await;

        assert!(
            matches!(
                app.screen(),
                Screen::Login {
                    notice: Some(Notice::Api(ApiError::SessionInvalid)),
                    ..
                }
            ),
            "{:?}",
            app.screen()
        );
        assert_eq!(app.session(), None);
        assert!(!app.can_go_back());
    });
}

#[test]
fn a_filter_that_fails_keeps_the_episodes_shown() {
    let (bridge, abs, proxy, failing) = bridge_behind_proxy();
    let device = Device::new();
    run(async || {
        let seeded = abs.seeded();
        let user = abs.new_user().await;
        let mut app = logged_in_behind(&device, &bridge, &proxy, &user).await;
        app.open_library(podcasts(&abs)).await;
        app.open_entry(&seeded.podcast).await;
        let Screen::Podcast { podcast, .. } = app.screen().clone() else {
            panic!("{:?}", app.screen());
        };

        *failing.lock().unwrap() = Some("GET /api/items/");
        app.filter_episodes(EpisodeFilter::Finished).await;

        assert_eq!(
            app.screen(),
            &Screen::Podcast {
                podcast,
                filter: EpisodeFilter::Unfinished,
                notice: Some(Notice::Api(ApiError::LibraryServerUnreachable)),
            }
        );
        assert!(app.session().is_some());
    });
}
