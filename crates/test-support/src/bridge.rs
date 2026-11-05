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
//! - [`run_until_exit`] is for a Bridge that must not start: it returns the exit status and
//!   every output line, stdout and stderr. JSON lines (log events) are objects; any other line
//!   becomes a [`Value::String`]. [`run_until_exit_with_key`] passes other key variables.
//! - A rejected call is checked with [`AppApi::get_error`] or [`AppApi::post_error`], which
//!   return the status and the decoded [`ApiError`] code.
//!
//! The Bridge's output is echoed to the test's output, so a failing test shows the Bridge's logs.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use api::{
    ApiError, CurrentUser, EpisodeFilter, Group, Item, Library, LibraryEntry, Podcast, Session,
};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::audiobookshelf::{AbsUser, Audiobookshelf};

/// Generous: the Bridge is up within milliseconds, but CI machines can be slow.
const TIMEOUT: Duration = Duration::from_secs(30);

/// A free loopback port, so tests can run in parallel.
pub const LISTEN: &str = "listen = \"127.0.0.1:0\"\n";

/// A database file in the Bridge's working directory.
pub const DATABASE: &str = "database = \"bridge.db\"\n";

/// Points the Bridge's Sonos discovery at a loopback port nothing answers on, so a test's Bridge
/// never searches the LAN it runs on.
pub const NO_SONOS: &str = "[sonos]\ndiscovery_address = \"127.0.0.1:9\"\n";

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
        output.extend(self.running.output.iter());
        output
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
/// unless `config` has a `[sonos]` section.
fn complete(config: &str) -> String {
    let sonos = if config.contains("[sonos]") {
        ""
    } else {
        NO_SONOS
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
pub fn sonos_toml(ssdp: std::net::SocketAddr) -> String {
    format!("[sonos]\ndiscovery_address = \"{ssdp}\"\n")
}

/// The rejection of a call without a usable session.
pub const SESSION_INVALID: (StatusCode, ApiError) =
    (StatusCode::UNAUTHORIZED, ApiError::SessionInvalid);

/// The rejection of a session call whose API key the Library Server rejects.
pub const KEY_REJECTED: (StatusCode, ApiError) =
    (StatusCode::UNAUTHORIZED, ApiError::ApiKeyRejected);

/// The rejection of an id that names nothing on the Library Server.
pub const NOT_FOUND: (StatusCode, ApiError) = (StatusCode::NOT_FOUND, ApiError::NotFound);

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

/// The App API as the App calls it, over HTTP. Paths and payloads follow the `api` crate's
/// server functions; responses decode into its types.
#[derive(Debug)]
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

    /// The Sonos Groups `session` sees.
    pub async fn groups(&self, session: &Session) -> Vec<Group> {
        self.get_as(session, "/api/groups", &[]).await
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
    let deadline = Instant::now() + TIMEOUT;
    let mut events = Vec::new();
    loop {
        match output.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(event) => events.push(event),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                stop(&mut child);
                panic!("Bridge still running after {TIMEOUT:?}; events: {events:#?}");
            }
        }
    }
    let status = child.wait().expect("wait for Bridge");
    Exited { status, events }
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
