//! The black-box Bridge harness.
//!
//! A test starts the real `bridge` binary as a subprocess and acts on it through the App API,
//! speaking its HTTP wire format (or, for the App's tests, through the App at [`TestBridge::url`]);
//! it also observes the exit status and log events, as an operator would. It never reaches into
//! the Bridge's modules or state.
//!
//! The binary is the `bridge` next to the test executable's directory (`target/<profile>/`).
//! The Bridge's own tests get it built by Cargo; other packages' tests need a
//! `cargo build -p bridge` first.
//!
//! ```ignore
//! let bridge = TestBridge::start(r#"
//!     [[library_servers]]
//!     url = "https://abs.example.com"
//! "#);
//! let servers = bridge.api().library_servers().await;
//! ```
//!
//! - [`TestBridge::start`] writes the configuration to a temporary directory, adding [`LISTEN`]
//!   (loopback, port 0, so tests can run in parallel without port clashes), [`DATABASE`] (a
//!   file in that directory, the Bridge's working directory) and, unless the test configures
//!   `[sonos]` (see [`sonos_toml`]), [`NO_SONOS`], and passes the encryption key [`KEY`] in
//!   `BRIDGE_ENCRYPTION_KEY`. [`TestBridge::start_with_key`] passes other [`KeyEnv`]
//!   variables: none, a key file (written to that directory), or both.
//! - Readiness comes from the Bridge itself: it logs a `listening` event with the address it
//!   bound, and `start` waits for that event. No sleeps.
//! - [`TestBridge::restart`] stops the Bridge and starts it again on the same configuration,
//!   key and database; [`TestBridge::restart_with`] and [`TestBridge::restart_with_key`]
//!   change the configuration or the key. Dropping the [`TestBridge`] kills the process.
//! - [`TestBridge::database_files`] reads the database file and its side files raw.
//! - [`TestBridge::signal`] sends the Bridge a [`Signal`] as an operator would;
//!   [`TestBridge::wait_for_event`] waits for a log event, and [`TestBridge::wait_for_exit`]
//!   for the Bridge to exit, returning its exit status and output as [`run_until_exit`] does.
//! - [`run_until_exit`] is for a Bridge that must not start: it returns the exit status and
//!   every output line, stdout and stderr. JSON lines (log events) are objects; any other line
//!   becomes a [`Value::String`]. [`run_until_exit_with_key`] passes other key variables.
//! - A rejected call is checked with [`AppApi::get_error`] or [`AppApi::post_error`], which
//!   return the status and the decoded [`ApiError`] code.
//! - [`clock_toml`] turns on the Bridge's test-only clock hook: the Bridge then times Handoff
//!   supervision (the Progress cadence, the end of an Item) on a `sonos_fake::Household`'s
//!   clock instead of the system clock. After advancing that clock, a test waits with
//!   `Household::clock_followed` until the Bridge has seen the new time, then waits on what
//!   it observes (Progress, Handoffs) with a deadline. Without the hook the Bridge runs on the
//!   system clock and a test's clock moves only the household.
//!
//! The Bridge's output is echoed to the test's output, so a failing test shows the Bridge's logs.

use std::io::{BufRead, BufReader, Read};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use api::{
    ApiError, CurrentUser, EpisodeFilter, Group, Handoff, Home, HomeEntry, Item, ItemId, Library,
    LibraryEntry, LocalStart, Playback, Podcast, Progress, Session, Skip,
};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::audiobookshelf::{AbsUser, Audiobookshelf, EPISODE_DURATIONS, EPISODE_PUBLISHED_AT};

/// Generous: the Bridge is up within milliseconds, but CI machines can be slow.
const TIMEOUT: Duration = Duration::from_secs(30);

/// A free loopback port, so tests can run in parallel.
pub const LISTEN: &str = "listen = \"127.0.0.1:0\"\n";

/// A database file in the Bridge's working directory.
pub const DATABASE: &str = "database = \"bridge.db\"\n";

/// Where a test's Bridge searches for Sonos players unless the test configures `[sonos]`: a
/// loopback port nothing answers on, so it never searches the LAN it runs on.
pub const NO_SONOS: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9));

/// The `RUST_LOG` the harness sets unless a test asks for another, so the developer's own
/// setting cannot hide the events the harness waits for.
pub const RUST_LOG: &str = "info";

/// The encryption key a Bridge starts with unless a test asks for another.
pub const KEY: &str = "BFpyGJHZOBgdHWxJN1ruoe2qmw9GlH9CJLJJNTkXQHo=";

/// Another valid encryption key.
pub const OTHER_KEY: &str = "twM8/bCobQ3lox9OulG0A+XcRBR0b6v1uk+Sz8Xk/ZI=";

/// The encryption key variables a Bridge starts with; unset when `None`. The developer's own
/// settings never reach the Bridge.
#[derive(Debug, Clone, Default)]
pub struct KeyEnv {
    /// `BRIDGE_ENCRYPTION_KEY_FILE`.
    pub file: Option<KeyFile>,
    /// `BRIDGE_ENCRYPTION_KEY`.
    pub value: Option<String>,
}

impl KeyEnv {
    /// Only `BRIDGE_ENCRYPTION_KEY`, set to `value`.
    pub fn value(value: &str) -> Self {
        Self {
            value: Some(value.to_owned()),
            ..Self::default()
        }
    }

    /// Only `BRIDGE_ENCRYPTION_KEY_FILE`, naming a file that holds `contents`.
    pub fn file(contents: &str) -> Self {
        Self {
            file: Some(KeyFile::Holding(contents.to_owned())),
            ..Self::default()
        }
    }
}

/// What `BRIDGE_ENCRYPTION_KEY_FILE` names.
#[derive(Debug, Clone)]
pub enum KeyFile {
    /// A file holding this, in the Bridge's directory.
    Holding(String),
    /// This, verbatim, as the variable's value: a path relative to the Bridge's directory where
    /// there may be no file, the empty string, or a mistake like the key itself.
    Named(String),
}

/// A running Bridge, killed on drop.
#[derive(Debug)]
pub struct TestBridge {
    running: Running,
    config: PathBuf,
    rust_log: String,
    key: KeyEnv,
    dir: TempDir,
}

/// One Bridge process, serving.
#[derive(Debug)]
struct Running {
    child: Child,
    output: Receiver<Value>,
    api: AppApi,
    startup: Vec<Value>,
    /// What [`TestBridge::wait_for_event`] received after `startup`.
    seen: Vec<Value>,
}

/// A signal asking the Bridge to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// SIGTERM, as `docker stop` sends.
    Terminate,
    /// SIGINT, as Ctrl-C in a terminal sends.
    Interrupt,
}

impl TestBridge {
    /// Starts a Bridge with `config` (TOML, without [`LISTEN`] and [`DATABASE`]) and waits
    /// until it serves. Panics if it exits or does not get ready in time.
    pub fn start(config: &str) -> Self {
        Self::start_logging(config, RUST_LOG)
    }

    /// [`TestBridge::start`] with the given `RUST_LOG`, which must let the `bridge` target's
    /// `listening` event through.
    pub fn start_logging(config: &str, rust_log: &str) -> Self {
        Self::new(config, rust_log, KeyEnv::value(KEY))
    }

    /// [`TestBridge::start`] with the encryption key variables `key`.
    pub fn start_with_key(config: &str, key: KeyEnv) -> Self {
        Self::new(config, RUST_LOG, key)
    }

    fn new(config: &str, rust_log: &str, key: KeyEnv) -> Self {
        let (dir, config) = config_file(&complete(config));
        Self {
            running: launch(&config, rust_log, &key),
            config,
            rust_log: rust_log.to_owned(),
            key,
            dir,
        }
    }

    /// Stops the Bridge and starts it again on the same configuration, key and database. It
    /// may listen on another port; [`TestBridge::api`] follows it.
    pub fn restart(&mut self) {
        stop(&mut self.running.child);
        self.running = launch(&self.config, &self.rust_log, &self.key);
    }

    /// [`TestBridge::restart`] with `config` (TOML, as for [`TestBridge::start`]) replacing
    /// the configuration; the database stays.
    pub fn restart_with(&mut self, config: &str) {
        std::fs::write(&self.config, complete(config)).expect("write config");
        self.restart();
    }

    /// [`TestBridge::restart`] with the encryption key variables `key`; the database stays.
    pub fn restart_with_key(&mut self, key: KeyEnv) {
        self.key = key;
        self.restart();
    }

    /// The path of the Bridge's database file, for a test that opens it alongside the Bridge.
    pub fn database_path(&self) -> PathBuf {
        self.dir.path().join("bridge.db")
    }

    /// The name and raw bytes of the database file and each of its side files (journal, WAL,
    /// shared memory) that exists.
    pub fn database_files(&self) -> Vec<(String, Vec<u8>)> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(self.dir.path()).expect("list the Bridge's directory") {
            let entry = entry.expect("directory entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("bridge.db") {
                files.push((
                    name,
                    std::fs::read(entry.path()).expect("read database file"),
                ));
            }
        }
        files
    }

    /// Stops the Bridge and returns everything it wrote since it last started.
    pub fn stop(mut self) -> Vec<Value> {
        stop(&mut self.running.child);
        let mut output = std::mem::take(&mut self.running.startup);
        output.append(&mut self.running.seen);
        collect_after_exit(&self.running.output, &mut output);
        output
    }

    /// Sends the Bridge `signal`.
    pub fn signal(&self, signal: Signal) {
        let name = match signal {
            Signal::Terminate => "-TERM",
            Signal::Interrupt => "-INT",
        };
        let status = Command::new("kill")
            .arg(name)
            .arg(self.running.child.id().to_string())
            .status()
            .expect("run kill");
        assert!(status.success(), "kill {name} failed: {status}");
    }

    /// Waits for the Bridge to log an event that `matches` accepts. Panics if the Bridge exits
    /// first or doesn't log one in time.
    pub fn wait_for_event(&mut self, matches: impl Fn(&Value) -> bool) {
        let running = &mut self.running;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match running
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(event) => {
                    let found = matches(&event);
                    running.seen.push(event);
                    if found {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => panic!(
                    "Bridge exited before the event: {:?}; output: {:#?}",
                    running.child.wait(),
                    running.seen
                ),
                Err(RecvTimeoutError::Timeout) => {
                    panic!(
                        "no such event within {TIMEOUT:?}; output: {:#?}",
                        running.seen
                    )
                }
            }
        }
    }

    /// Waits for the Bridge to exit; returns its exit status and everything it wrote since it
    /// last started. Panics if it is still running after the timeout. [`TestBridge::restart`]
    /// starts it again.
    pub fn wait_for_exit(&mut self) -> Exited {
        let running = &mut self.running;
        let mut events = running.startup.clone();
        events.append(&mut running.seen);
        until_exit(&mut running.child, &running.output, events)
    }

    /// A client for this Bridge's App API.
    pub fn api(&self) -> &AppApi {
        &self.running.api
    }

    /// The Bridge's base URL, as a person would enter it in the App.
    pub fn url(&self) -> &str {
        &self.running.api.base_url
    }

    /// Everything the Bridge wrote up to and including its `listening` event.
    pub fn startup_output(&self) -> &[Value] {
        &self.running.startup
    }
}

/// The configuration file for `config`: [`LISTEN`], [`DATABASE`], `config`, and [`NO_SONOS`]
/// unless `config` sets `discovery_address` in `[sonos]`. A `[sonos]` without it would search
/// the LAN, so it panics. Configuration that isn't TOML is passed on as it is, for the Bridge to
/// reject.
fn complete(config: &str) -> String {
    let sonos = match config.parse::<toml::Table>() {
        Ok(table) => match table.get("sonos") {
            Some(sonos) => {
                assert!(
                    sonos.get("discovery_address").is_some(),
                    "a test's [sonos] must set discovery_address: {config}"
                );
                String::new()
            }
            None => sonos_toml(NO_SONOS),
        },
        Err(_) => String::new(),
    };
    format!("{LISTEN}{DATABASE}{config}{sonos}")
}

impl Drop for TestBridge {
    fn drop(&mut self) {
        stop(&mut self.running.child);
    }
}

/// Starts a Bridge and waits for its `listening` event.
fn launch(config: &Path, rust_log: &str, key: &KeyEnv) -> Running {
    let (mut child, output) = spawn(config, rust_log, key);
    let deadline = Instant::now() + TIMEOUT;
    let mut startup = Vec::new();
    loop {
        match output.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(event) if is_listening(&event) => {
                let Some(addr) = event["addr"].as_str() else {
                    stop(&mut child);
                    panic!("listening event has no addr: {event}");
                };
                let api = AppApi {
                    http: reqwest::Client::new(),
                    base_url: format!("http://{addr}"),
                };
                startup.push(event);
                return Running {
                    child,
                    output,
                    api,
                    startup,
                    seen: Vec::new(),
                };
            }
            Ok(event) => startup.push(event),
            Err(RecvTimeoutError::Disconnected) => {
                panic!(
                    "Bridge exited before serving: {:?}; output: {startup:#?}",
                    child.wait()
                );
            }
            Err(RecvTimeoutError::Timeout) => {
                stop(&mut child);
                panic!("Bridge not ready within {TIMEOUT:?}; output: {startup:#?}");
            }
        }
    }
}

/// Whether `event` is the Bridge announcing it serves the App API.
pub fn is_listening(event: &Value) -> bool {
    event["message"] == "listening"
}

/// The `Authorization` header value that authenticates as `session`.
fn bearer(session: &Session) -> String {
    format!("Bearer {}", session.token.expose())
}

/// The configuration entry for a Library Server at `url`.
pub fn library_server_toml(url: &str) -> String {
    format!("[[library_servers]]\nurl = \"{url}\"\n")
}

/// The configuration entry for a Library Server at `url` named `name`.
pub fn named_library_server_toml(url: &str, name: &str) -> String {
    format!("{}name = \"{name}\"\n", library_server_toml(url))
}

/// The configuration section pointing the Bridge's Sonos discovery at `ssdp`, such as a
/// `sonos_fake::Household`'s address.
pub fn sonos_toml(ssdp: SocketAddr) -> String {
    format!("[sonos]\ndiscovery_address = \"{ssdp}\"\n")
}

/// The configuration section turning on the Bridge's test-only clock hook: it follows the
/// clock a `sonos_fake::Household` serves at `clock` (`Household::clock_address`).
pub fn clock_toml(clock: SocketAddr) -> String {
    format!("[testing]\nclock_address = \"{clock}\"\n")
}

/// The configuration naming the Library Server at `url` "Home" and pointing Sonos discovery at
/// `ssdp`.
pub fn home_with_sonos_toml(url: &str, ssdp: SocketAddr) -> String {
    format!(
        "{}{}",
        named_library_server_toml(url, "Home"),
        sonos_toml(ssdp)
    )
}

/// The rejection of a call without a usable session.
pub const SESSION_INVALID: (StatusCode, ApiError) =
    (StatusCode::UNAUTHORIZED, ApiError::SessionInvalid);

/// The rejection of a Groups listing no Sonos player answered.
pub const SONOS_UNREACHABLE: (StatusCode, ApiError) =
    (StatusCode::BAD_GATEWAY, ApiError::SonosUnreachable);

/// The rejection of a Groups listing when the Bridge could not search for Sonos players.
pub const SONOS_SEARCH_FAILED: (StatusCode, ApiError) =
    (StatusCode::BAD_GATEWAY, ApiError::SonosSearchFailed);

/// The rejection of a session call whose API key the Library Server rejects.
pub const KEY_REJECTED: (StatusCode, ApiError) =
    (StatusCode::UNAUTHORIZED, ApiError::ApiKeyRejected);

/// The rejection of an id that names nothing on the Library Server.
pub const NOT_FOUND: (StatusCode, ApiError) = (StatusCode::NOT_FOUND, ApiError::NotFound);

/// The rejection of a call naming a Group the household doesn't have.
pub const GROUP_NOT_FOUND: (StatusCode, ApiError) =
    (StatusCode::NOT_FOUND, ApiError::GroupNotFound);

/// The rejection of a call whose Group didn't answer in time, or refused.
pub const GROUP_FAILED: (StatusCode, ApiError) = (StatusCode::BAD_GATEWAY, ApiError::GroupFailed);

/// The rejection of a control for a Group without a Handoff it plays.
pub const NO_HANDOFF: (StatusCode, ApiError) = (StatusCode::CONFLICT, ApiError::NoHandoff);

/// The rejection of a forward skip on an Item whose length neither the Group nor the Library
/// Server knows.
pub const LENGTH_UNKNOWN: (StatusCode, ApiError) = (StatusCode::CONFLICT, ApiError::LengthUnknown);

/// The rejection of a call whose Library Server couldn't be reached, or didn't answer in time.
pub const LIBRARY_SERVER_UNREACHABLE: (StatusCode, ApiError) =
    (StatusCode::BAD_GATEWAY, ApiError::LibraryServerUnreachable);

/// The rejection of a Local playback report or end without a Local session.
pub const NO_LOCAL_SESSION: (StatusCode, ApiError) =
    (StatusCode::CONFLICT, ApiError::NoLocalSession);

/// The rejection of an Item the Bridge can't stream yet.
pub const ITEM_NOT_PLAYABLE: (StatusCode, ApiError) =
    (StatusCode::UNPROCESSABLE_ENTITY, ApiError::ItemNotPlayable);

/// Who a session of `user` on `abs` acts as.
pub fn acting_as(abs: &Audiobookshelf, user: &AbsUser) -> CurrentUser {
    acting_on(abs.base_url(), user)
}

/// Who a session of `user` on the Library Server at `library_server` acts as.
pub fn acting_on(library_server: &str, user: &AbsUser) -> CurrentUser {
    CurrentUser {
        library_server: library_server.to_owned(),
        user_id: user.id.clone(),
        username: user.username.clone(),
    }
}

/// The seeded "The Long Book" as Home lists it, with `progress`.
pub fn home_long_book(abs: &Audiobookshelf, progress: Option<Progress>) -> HomeEntry {
    HomeEntry {
        id: ItemId {
            library_item: abs.seeded().long_book.clone(),
            episode: None,
        },
        title: "The Long Book".into(),
        author: Some("Pratchett".into()),
        podcast: None,
        duration: Some(4320.0),
        published_at: None,
        progress,
    }
}

/// The seeded episode `n` (0 is the oldest) as Home lists it, with `progress`.
pub fn home_episode(abs: &Audiobookshelf, n: usize, progress: Option<Progress>) -> HomeEntry {
    let seeded = abs.seeded();
    HomeEntry {
        id: ItemId {
            library_item: seeded.podcast.clone(),
            episode: Some(seeded.episodes[n].clone()),
        },
        title: format!("Episode {}", n + 1),
        author: None,
        podcast: Some("The Clacks Hour".into()),
        duration: Some(EPISODE_DURATIONS[n]),
        published_at: Some(EPISODE_PUBLISHED_AT[n]),
        progress,
    }
}

/// The App API as the App calls it, over HTTP. Paths and payloads follow the `api` crate's
/// server functions; responses decode into its types.
#[derive(Debug, Clone)]
pub struct AppApi {
    http: reqwest::Client,
    base_url: String,
}

impl AppApi {
    pub async fn library_servers(&self) -> Vec<api::LibraryServer> {
        self.get("/api/library-servers").await
    }

    /// Logs in, panicking unless that succeeds.
    pub async fn login(&self, library_server: &str, api_key: &str) -> Session {
        decode(
            self.post("/api/login", None, login_body(library_server, api_key))
                .await,
        )
        .await
    }

    /// Logs in, expecting a rejection.
    pub async fn login_error(&self, library_server: &str, api_key: &str) -> (StatusCode, ApiError) {
        self.post_error("/api/login", None, login_body(library_server, api_key))
            .await
    }

    pub async fn current_user(&self, session: &Session) -> CurrentUser {
        self.get_as(session, "/api/me", &[]).await
    }

    /// Asks who `session` acts for, expecting a rejection.
    pub async fn current_user_error(&self, session: &Session) -> (StatusCode, ApiError) {
        self.get_as_error(session, "/api/me", &[]).await
    }

    pub async fn logout(&self, session: &Session) {
        let response = self
            .post("/api/logout", Some(&bearer(session)), json!({}))
            .await;
        decode::<Value>(response).await;
    }

    /// Logs `session` out, expecting a rejection.
    pub async fn logout_error(&self, session: &Session) -> (StatusCode, ApiError) {
        self.post_error("/api/logout", Some(&bearer(session)), json!({}))
            .await
    }

    /// The Libraries `session` sees.
    pub async fn libraries(&self, session: &Session) -> Vec<Library> {
        self.get_as(session, "/api/libraries", &[]).await
    }

    /// The audiobooks or podcasts in Library `library`, as `session` sees them.
    pub async fn library_entries(&self, session: &Session, library: &str) -> Vec<LibraryEntry> {
        self.get_as(session, "/api/library-entries", &[("library", library)])
            .await
    }

    /// The audiobook `library_item`, or its episode `episode`, as `session` sees it.
    pub async fn item(&self, session: &Session, library_item: &str, episode: Option<&str>) -> Item {
        self.get_as(session, "/api/item", &item_query(library_item, episode))
            .await
    }

    /// Podcast `id` with the episodes `filter` picks, as `session` sees it.
    pub async fn podcast(&self, session: &Session, id: &str, filter: EpisodeFilter) -> Podcast {
        self.get_as(
            session,
            "/api/podcast",
            &[("id", id), ("filter", &filter_name(filter))],
        )
        .await
    }

    /// Home as `session` sees it.
    pub async fn home(&self, session: &Session) -> Home {
        self.get_as(session, "/api/home", &[]).await
    }

    /// Asks for Home as `session`, expecting a rejection.
    pub async fn home_error(&self, session: &Session) -> (StatusCode, ApiError) {
        self.get_as_error(session, "/api/home", &[]).await
    }

    /// Marks Item `item` Finished (`finished`) or unfinished as `session`, panicking unless
    /// that succeeds; the Item as the Bridge answers.
    pub async fn mark_finished(&self, session: &Session, item: &ItemId, finished: bool) -> Item {
        let response = self
            .post(
                "/api/item/finished",
                Some(&bearer(session)),
                mark_finished_body(item, finished),
            )
            .await;
        decode(response).await
    }

    /// Marks Item `item` Finished (`finished`) or unfinished as `session`, expecting a
    /// rejection.
    pub async fn mark_finished_error(
        &self,
        session: &Session,
        item: &ItemId,
        finished: bool,
    ) -> (StatusCode, ApiError) {
        self.post_error(
            "/api/item/finished",
            Some(&bearer(session)),
            mark_finished_body(item, finished),
        )
        .await
    }

    /// The Sonos Groups `session` sees.
    pub async fn groups(&self, session: &Session) -> Vec<Group> {
        self.get_as(session, "/api/groups", &[]).await
    }

    /// Starts Item `item` on Group `group` as `session`, at the session user's Progress,
    /// panicking unless that succeeds.
    pub async fn start_handoff(&self, session: &Session, item: &ItemId, group: &str) -> Handoff {
        self.start_handoff_with(session, handoff_body(item, group, false))
            .await
    }

    /// Starts Item `item` on Group `group` as `session` from its beginning ("Play from
    /// start"), panicking unless that succeeds.
    pub async fn start_handoff_from_start(
        &self,
        session: &Session,
        item: &ItemId,
        group: &str,
    ) -> Handoff {
        self.start_handoff_with(session, handoff_body(item, group, true))
            .await
    }

    /// Starts a Handoff as `session` with request body `body`, panicking unless that
    /// succeeds.
    pub async fn start_handoff_with(&self, session: &Session, body: Value) -> Handoff {
        let response = self
            .post("/api/handoffs/start", Some(&bearer(session)), body)
            .await;
        decode(response).await
    }

    /// Starts Item `item` on Group `group` as `session`, expecting a rejection.
    pub async fn start_handoff_error(
        &self,
        session: &Session,
        item: &ItemId,
        group: &str,
    ) -> (StatusCode, ApiError) {
        self.post_error(
            "/api/handoffs/start",
            Some(&bearer(session)),
            handoff_body(item, group, false),
        )
        .await
    }

    /// Starts Item `item` on Group `group` as `session` from its beginning ("Play from
    /// start"), expecting a rejection.
    pub async fn start_handoff_from_start_error(
        &self,
        session: &Session,
        item: &ItemId,
        group: &str,
    ) -> (StatusCode, ApiError) {
        self.post_error(
            "/api/handoffs/start",
            Some(&bearer(session)),
            handoff_body(item, group, true),
        )
        .await
    }

    /// The active Handoffs `session` sees.
    pub async fn handoffs(&self, session: &Session) -> Vec<Handoff> {
        self.get_as(session, "/api/handoffs", &[]).await
    }

    /// Pauses Group `group` as `session`, panicking unless that succeeds.
    pub async fn pause_group(&self, session: &Session, group: &str) -> Playback {
        self.control(session, group, GroupControl::Pause).await
    }

    /// Resumes Group `group` as `session`, panicking unless that succeeds.
    pub async fn resume_group(&self, session: &Session, group: &str) -> Playback {
        self.control(session, group, GroupControl::Resume).await
    }

    /// Skips Group `group` by `skip` as `session`, panicking unless that succeeds.
    pub async fn skip_group(&self, session: &Session, group: &str, skip: Skip) -> Playback {
        self.control(session, group, GroupControl::Skip(skip)).await
    }

    /// Sets Group `group`'s volume as `session`, panicking unless that succeeds; the volume
    /// set.
    pub async fn set_group_volume(&self, session: &Session, group: &str, volume: u8) -> u8 {
        self.control(session, group, GroupControl::Volume(volume))
            .await
    }

    /// Makes `control` on Group `group` as `session`, panicking unless that succeeds.
    pub async fn control<T: serde::de::DeserializeOwned>(
        &self,
        session: &Session,
        group: &str,
        control: GroupControl,
    ) -> T {
        let (path, body) = control.request(group);
        decode(self.post(path, Some(&bearer(session)), body).await).await
    }

    /// Makes `control` on Group `group` as `session`, expecting a rejection.
    pub async fn control_error(
        &self,
        session: &Session,
        group: &str,
        control: GroupControl,
    ) -> (StatusCode, ApiError) {
        let (path, body) = control.request(group);
        self.post_error(path, Some(&bearer(session)), body).await
    }

    /// Makes `control` on Group `group` without a session, expecting a rejection.
    pub async fn control_error_unauthenticated(
        &self,
        group: &str,
        control: GroupControl,
    ) -> (StatusCode, ApiError) {
        let (path, body) = control.request(group);
        self.post_error(path, None, body).await
    }

    /// Starts Local playback of Item `item` as `session`, panicking unless that succeeds.
    pub async fn start_local(&self, session: &Session, item: &ItemId) -> LocalStart {
        let body = json!({"item": item});
        decode(
            self.post("/api/local/start", Some(&bearer(session)), body)
                .await,
        )
        .await
    }

    /// Starts Local playback of Item `item` as `session`, expecting a rejection.
    pub async fn start_local_error(
        &self,
        session: &Session,
        item: &ItemId,
    ) -> (StatusCode, ApiError) {
        let body = json!({"item": item});
        self.post_error("/api/local/start", Some(&bearer(session)), body)
            .await
    }

    /// Reports `position` (and `duration`) of `session`'s Local playback whose stream's path
    /// is `stream`, or ends it (`end`), panicking unless that succeeds.
    pub async fn report_local(
        &self,
        session: &Session,
        report: LocalReport,
        stream: &str,
        position: f64,
        duration: Option<f64>,
    ) {
        let body = local_report_body(stream, position, duration);
        decode::<Value>(self.post(report.path(), Some(&bearer(session)), body).await).await;
    }

    /// [`AppApi::report_local`], expecting a rejection.
    pub async fn report_local_error(
        &self,
        session: &Session,
        report: LocalReport,
        stream: &str,
        position: f64,
        duration: Option<f64>,
    ) -> (StatusCode, ApiError) {
        let body = local_report_body(stream, position, duration);
        self.post_error(report.path(), Some(&bearer(session)), body)
            .await
    }

    /// `GET`s (or with `method`, `HEAD`s) `path` on the Bridge without authenticating, with
    /// the `Range` header `range`, if any, as a Group fetches a stream.
    pub async fn fetch(
        &self,
        method: Method,
        path: &str,
        range: Option<&str>,
    ) -> reqwest::Response {
        let mut request = self
            .http
            .request(method.clone(), format!("{}{path}", self.base_url));
        if let Some(range) = range {
            request = request.header(reqwest::header::RANGE, range);
        }
        request
            .send()
            .await
            .unwrap_or_else(|error| panic!("{method} {path}: {error}"))
    }

    /// GETs `path` with `query` as `session` and decodes the successful response body, typed
    /// or as raw JSON.
    pub async fn get_as<T: serde::de::DeserializeOwned>(
        &self,
        session: &Session,
        path: &str,
        query: &[(&str, &str)],
    ) -> T {
        let response = self
            .send(Method::GET, path, query, Some(&bearer(session)), None)
            .await;
        decode(response).await
    }

    /// GETs `path` with `query` as `session`, expecting a rejection.
    pub async fn get_as_error(
        &self,
        session: &Session,
        path: &str,
        query: &[(&str, &str)],
    ) -> (StatusCode, ApiError) {
        self.get_error(path, query, Some(&bearer(session))).await
    }

    /// GETs `path` and decodes the successful response body, typed or as raw JSON.
    pub async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        decode(self.send(Method::GET, path, &[], None, None).await).await
    }

    /// POSTs `body` to `path` with the given `Authorization` header, if any.
    pub async fn post(
        &self,
        path: &str,
        authorization: Option<&str>,
        body: Value,
    ) -> reqwest::Response {
        self.send(Method::POST, path, &[], authorization, Some(body))
            .await
    }

    /// GETs `path` with `query` and the given `Authorization` header, if any, expecting a
    /// rejection.
    pub async fn get_error(
        &self,
        path: &str,
        query: &[(&str, &str)],
        authorization: Option<&str>,
    ) -> (StatusCode, ApiError) {
        api_error(
            self.send(Method::GET, path, query, authorization, None)
                .await,
        )
        .await
    }

    /// POSTs `body` to `path` with the given `Authorization` header, if any, expecting a
    /// rejection.
    pub async fn post_error(
        &self,
        path: &str,
        authorization: Option<&str>,
        body: Value,
    ) -> (StatusCode, ApiError) {
        api_error(self.post(path, authorization, body).await).await
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        authorization: Option<&str>,
        body: Option<Value>,
    ) -> reqwest::Response {
        let mut request = self
            .http
            .request(method.clone(), format!("{}{path}", self.base_url))
            .query(query);
        if let Some(authorization) = authorization {
            request = request.header(reqwest::header::AUTHORIZATION, authorization);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        request
            .send()
            .await
            .unwrap_or_else(|error| panic!("{method} {path}: {error}"))
    }
}

/// The query naming an Item.
pub fn item_query<'a>(library_item: &'a str, episode: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
    let mut query = vec![("library_item", library_item)];
    query.extend(episode.map(|episode| ("episode", episode)));
    query
}

/// How an [`EpisodeFilter`] travels in a query: as it serializes.
pub fn filter_name(filter: EpisodeFilter) -> String {
    match serde_json::to_value(filter) {
        Ok(Value::String(name)) => name,
        other => panic!("{filter:?} serializes as {other:?}"),
    }
}

fn handoff_body(item: &ItemId, group: &str, from_start: bool) -> Value {
    json!({"item": item, "group": group, "from_start": from_start})
}

/// The body of a Local playback report or end.
pub fn local_report_body(stream: &str, position: f64, duration: Option<f64>) -> Value {
    json!({"stream": stream, "position": position, "duration": duration})
}

/// The id and the (percent-encoded) file name in a stream's path, `/stream/<id>/<file>`: a
/// Handoff's or a Local session's.
fn stream_path_parts(path: &str) -> (&str, &str) {
    path.strip_prefix("/stream/")
        .and_then(|rest| rest.split_once('/'))
        .unwrap_or_else(|| panic!("not a stream path: {path}"))
}

/// The id in a stream's path, `/stream/<id>/<file>`.
pub fn stream_path_id(path: &str) -> &str {
    stream_path_parts(path).0
}

/// The (percent-encoded) file name in a stream's path, `/stream/<id>/<file>`.
pub fn stream_path_file(path: &str) -> &str {
    stream_path_parts(path).1
}

/// How the App tells the Bridge where Local playback is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalReport {
    /// [`api::report_local`]: the Local session goes on.
    Report,
    /// [`api::end_local`]: the Local session ends.
    End,
}

impl LocalReport {
    fn path(self) -> &'static str {
        match self {
            Self::Report => "/api/local/report",
            Self::End => "/api/local/end",
        }
    }
}

/// A call that controls a Group, as the App makes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupControl {
    Pause,
    Resume,
    Skip(Skip),
    Volume(u8),
}

impl GroupControl {
    /// Its path and body for Group `group`.
    fn request(self, group: &str) -> (&'static str, Value) {
        match self {
            Self::Pause => ("/api/groups/pause", json!({"group": group})),
            Self::Resume => ("/api/groups/resume", json!({"group": group})),
            Self::Skip(skip) => ("/api/groups/skip", json!({"group": group, "skip": skip})),
            Self::Volume(volume) => (
                "/api/groups/volume",
                json!({"group": group, "volume": volume}),
            ),
        }
    }
}

/// The body that marks Item `item` Finished (`finished`) or unfinished.
pub fn mark_finished_body(item: &ItemId, finished: bool) -> Value {
    json!({"item": item, "finished": finished})
}

fn login_body(library_server: &str, api_key: &str) -> Value {
    json!({"library_server": library_server, "api_key": api_key})
}

/// The successful response's body, decoded; panics with the body if the call failed.
async fn decode<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> T {
    let status = response.status();
    let url = response.url().path().to_owned();
    let body = response.text().await.expect("response body");
    assert!(status.is_success(), "{url}: {status}: {body}");
    serde_json::from_str(&body).unwrap_or_else(|error| panic!("decode {url}: {error}: {body}"))
}

/// The status and error code of a rejected call. Panics if the call succeeded or its body is
/// not an App API error.
async fn api_error(response: reqwest::Response) -> (StatusCode, ApiError) {
    let status = response.status();
    let url = response.url().path().to_owned();
    let mut body: Value = response
        .json()
        .await
        .unwrap_or_else(|error| panic!("{url}: {status}: body is not JSON: {error}"));
    assert!(!status.is_success(), "{url} succeeded: {body}");
    let code = serde_json::from_value(body["data"].take())
        .unwrap_or_else(|error| panic!("{url}: {status}: no error code: {error}: {body}"));
    (status, code)
}

/// How a Bridge that was expected to stop ended.
#[derive(Debug)]
pub struct Exited {
    pub status: ExitStatus,
    /// When the process was seen to have exited (within a few milliseconds), for timing a stop.
    pub at: Instant,
    /// Every line the Bridge wrote, stdout and stderr interleaved as they arrived.
    pub events: Vec<Value>,
}

impl Exited {
    pub fn errors(&self) -> impl Iterator<Item = &Value> {
        self.events.iter().filter(|event| event["level"] == "ERROR")
    }

    /// Whether `text` appears anywhere in what the Bridge wrote.
    pub fn output_contains(&self, text: &str) -> bool {
        output_contains(&self.events, text)
    }
}

/// Whether `text` appears anywhere in `output`, lines a Bridge wrote.
pub fn output_contains(output: &[Value], text: &str) -> bool {
    output.iter().any(|event| match event {
        Value::String(line) => line.contains(text),
        event => event.to_string().contains(text),
    })
}

/// Runs a Bridge with the configuration file at `config` (which may be missing), the given
/// `RUST_LOG` and the encryption key [`KEY`], and waits for it to exit. Panics if it is still
/// running after the timeout.
pub fn run_until_exit(config: &Path, rust_log: &str) -> Exited {
    run_until_exit_with_key(config, rust_log, &KeyEnv::value(KEY))
}

/// [`run_until_exit`] with the encryption key variables `key`.
pub fn run_until_exit_with_key(config: &Path, rust_log: &str, key: &KeyEnv) -> Exited {
    let (mut child, output) = spawn(config, rust_log, key);
    until_exit(&mut child, &output, Vec::new())
}

/// Collects `child`'s `output` after `events` until it exits, then returns how it ended. Kills
/// it and panics if it is still running after the timeout.
///
/// Exit is the process's, not end of file on its pipes: a child spawned concurrently elsewhere
/// in the test process can inherit a pipe's write end (on macOS std sets close-on-exec on a new
/// pipe only after creating it) and hold end of file back for as long as it runs. Output that
/// arrives within [`OUTPUT_AFTER_EXIT`] of the exit is kept.
fn until_exit(child: &mut Child, output: &Receiver<Value>, mut events: Vec<Value>) -> Exited {
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("check whether the Bridge exited") {
            break status;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            stop(child);
            panic!("Bridge still running after {TIMEOUT:?}; events: {events:#?}");
        }
        match output.recv_timeout(left.min(EXIT_POLL)) {
            Ok(event) => events.push(event),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break child.wait().expect("wait for Bridge"),
        }
    };
    let at = Instant::now();
    collect_after_exit(output, &mut events);
    Exited { status, at, events }
}

/// How often [`until_exit`] checks whether the Bridge has exited.
const EXIT_POLL: Duration = Duration::from_millis(5);

/// How long output is still collected once the Bridge has exited: everything it wrote is
/// already in its pipes, so this only bounds the wait for end of file (see [`until_exit`]).
const OUTPUT_AFTER_EXIT: Duration = Duration::from_secs(1);

/// Appends to `events` what an exited Bridge's `output` still holds, until end of file or
/// [`OUTPUT_AFTER_EXIT`].
fn collect_after_exit(output: &Receiver<Value>, events: &mut Vec<Value>) {
    let deadline = Instant::now() + OUTPUT_AFTER_EXIT;
    while let Ok(event) = output.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        events.push(event);
    }
}

/// A temporary directory holding `contents` as the config file `bridge.toml`.
pub fn config_file(contents: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("bridge.toml");
    std::fs::write(&path, contents).expect("write config");
    (dir, path)
}

/// The `bridge` binary in the Cargo target directory this test executable was built into.
fn bridge_binary() -> PathBuf {
    let test_exe = std::env::current_exe().expect("path of the test executable");
    // `target/<profile>/deps/<test>` → `target/<profile>/bridge`
    let binary = test_exe
        .parent()
        .and_then(Path::parent)
        .expect("test executable in target/<profile>/deps")
        .join(format!("bridge{}", std::env::consts::EXE_SUFFIX));
    assert!(
        binary.is_file(),
        "no Bridge binary at {}: run `cargo build -p bridge` first",
        binary.display()
    );
    binary
}

/// Starts the Bridge in the config file's directory and forwards its output lines until it
/// closes stdout and stderr (exits). A key file is written to that directory.
fn spawn(config: &Path, rust_log: &str, key: &KeyEnv) -> (Child, Receiver<Value>) {
    let dir = config.parent().expect("config file in a directory");
    let mut command = Command::new(bridge_binary());
    command
        .arg("--config")
        .arg(config)
        .current_dir(dir)
        .env("RUST_LOG", rust_log)
        .env_remove("BRIDGE_CONFIG")
        .env_remove("BRIDGE_ENCRYPTION_KEY_FILE")
        .env_remove("BRIDGE_ENCRYPTION_KEY")
        // Dioxus reads static files from here; a developer's setting must not change the logs.
        .env_remove("DIOXUS_PUBLIC_PATH")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match &key.file {
        Some(KeyFile::Holding(contents)) => {
            let path = dir.join("encryption-key");
            std::fs::write(&path, contents).expect("write key file");
            command.env("BRIDGE_ENCRYPTION_KEY_FILE", path);
        }
        Some(KeyFile::Named(value)) => {
            command.env("BRIDGE_ENCRYPTION_KEY_FILE", value);
        }
        None => {}
    }
    if let Some(value) = &key.value {
        command.env("BRIDGE_ENCRYPTION_KEY", value);
    }
    let mut child = command.spawn().expect("spawn bridge");
    let (sender, receiver) = mpsc::channel();
    forward(child.stdout.take().expect("piped stdout"), sender.clone());
    forward(child.stderr.take().expect("piped stderr"), sender);
    (child, receiver)
}

/// Sends each line of `stream` as JSON, or as a string when it is not JSON (or not UTF-8).
fn forward(stream: impl Read + Send + 'static, sender: Sender<Value>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut bytes = Vec::new();
        while reader
            .read_until(b'\n', &mut bytes)
            .is_ok_and(|read| read > 0)
        {
            let line = String::from_utf8_lossy(&bytes).trim_end().to_owned();
            bytes.clear();
            eprintln!("[bridge] {line}");
            let event = serde_json::from_str(&line).unwrap_or(Value::String(line));
            // Keep draining after the receiver is gone, so the Bridge never blocks on a full pipe.
            let _ = sender.send(event);
        }
    });
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
