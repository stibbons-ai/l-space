//! The black-box Bridge harness.
//!
//! A test starts the real `bridge` binary as a subprocess and acts on it through the App API,
//! speaking its HTTP wire format; it also observes the exit status and log events, as an
//! operator would. It never reaches into the Bridge's modules or state.
//!
//! ```ignore
//! let bridge = TestBridge::start(r#"
//!     [[library_servers]]
//!     url = "https://abs.example.com"
//! "#);
//! let servers = bridge.api().library_servers().await;
//! ```
//!
//! - [`TestBridge::start`] writes the configuration to a temporary directory, adding
//!   [`REQUIRED`]: a `listen` address on loopback with port 0, so tests can run in parallel
//!   without port clashes, and a `database` in that directory, the Bridge's working directory.
//! - Readiness comes from the Bridge itself: it logs a `listening` event with the address it
//!   bound, and `start` waits for that event. No sleeps.
//! - [`TestBridge::restart`] stops the Bridge and starts it again on the same configuration
//!   and database. Dropping the [`TestBridge`] kills the process.
//! - [`run_until_exit`] is for a Bridge that must not start: it returns the exit status and
//!   every output line, stdout and stderr. JSON lines (log events) are objects; any other line
//!   becomes a [`Value::String`].
//! - A rejected call is checked with [`AppApi::get_error`] or [`AppApi::post_error`], which
//!   return the status and the decoded [`ApiError`] code.
//!
//! The Bridge's output is echoed to the test's output, so a failing test shows the Bridge's logs.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use api::{ApiError, CurrentUser, Session};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;

/// Generous: the Bridge is up within milliseconds, but CI machines can be slow.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The settings every configuration needs: a free loopback port, so tests can run in parallel,
/// and a database file in the Bridge's working directory.
pub const REQUIRED: &str = "listen = \"127.0.0.1:0\"\ndatabase = \"bridge.db\"\n";

/// The `RUST_LOG` the harness sets unless a test asks for another, so the developer's own
/// setting cannot hide the events the harness waits for.
pub const RUST_LOG: &str = "info";

/// A running Bridge, killed on drop.
pub struct TestBridge {
    child: Child,
    output: Receiver<Value>,
    api: AppApi,
    startup: Vec<Value>,
    config: PathBuf,
    rust_log: String,
    _dir: TempDir,
}

impl TestBridge {
    /// Starts a Bridge with `config` (TOML, without [`REQUIRED`]) and waits until it serves.
    /// Panics if it exits or does not get ready in time.
    pub fn start(config: &str) -> Self {
        Self::start_logging(config, RUST_LOG)
    }

    /// [`TestBridge::start`] with the given `RUST_LOG`, which must let the `bridge` target's
    /// `listening` event through.
    pub fn start_logging(config: &str, rust_log: &str) -> Self {
        let (dir, config) = config_file(&format!("{REQUIRED}{config}"));
        let (child, output, api, startup) = launch(&config, rust_log);
        Self {
            child,
            output,
            api,
            startup,
            config,
            rust_log: rust_log.to_owned(),
            _dir: dir,
        }
    }

    /// Stops the Bridge and starts it again on the same configuration and database. It may
    /// listen on another port; [`TestBridge::api`] follows it.
    pub fn restart(&mut self) {
        stop(&mut self.child);
        (self.child, self.output, self.api, self.startup) = launch(&self.config, &self.rust_log);
    }

    /// Stops the Bridge and returns everything it wrote since it last started.
    pub fn stop(mut self) -> Vec<Value> {
        stop(&mut self.child);
        let mut output = std::mem::take(&mut self.startup);
        output.extend(self.output.iter());
        output
    }

    /// A client for this Bridge's App API.
    pub fn api(&self) -> &AppApi {
        &self.api
    }

    /// Everything the Bridge wrote up to and including its `listening` event.
    pub fn startup_output(&self) -> &[Value] {
        &self.startup
    }
}

impl Drop for TestBridge {
    fn drop(&mut self) {
        stop(&mut self.child);
    }
}

/// Starts a Bridge and waits for its `listening` event.
fn launch(config: &Path, rust_log: &str) -> (Child, Receiver<Value>, AppApi, Vec<Value>) {
    let (mut child, output) = spawn(config, rust_log);
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
                return (child, output, api, startup);
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
pub fn bearer(session: &Session) -> String {
    format!("Bearer {}", session.token.expose())
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
        let response = self
            .send(Method::GET, "/api/me", Some(&bearer(session)), None)
            .await;
        decode(response).await
    }

    pub async fn logout(&self, session: &Session) {
        let response = self
            .post("/api/logout", Some(&bearer(session)), json!({}))
            .await;
        decode::<Value>(response).await;
    }

    /// GETs `path` and decodes the successful response body, typed or as raw JSON.
    pub async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        decode(self.send(Method::GET, path, None, None).await).await
    }

    /// POSTs `body` to `path` with the given `Authorization` header, if any.
    pub async fn post(
        &self,
        path: &str,
        authorization: Option<&str>,
        body: Value,
    ) -> reqwest::Response {
        self.send(Method::POST, path, authorization, Some(body))
            .await
    }

    /// GETs `path` with the given `Authorization` header, if any, expecting a rejection.
    pub async fn get_error(
        &self,
        path: &str,
        authorization: Option<&str>,
    ) -> (StatusCode, ApiError) {
        api_error(self.send(Method::GET, path, authorization, None).await).await
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
        authorization: Option<&str>,
        body: Option<Value>,
    ) -> reqwest::Response {
        let mut request = self
            .http
            .request(method.clone(), format!("{}{path}", self.base_url));
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
    let body: Value = response
        .json()
        .await
        .unwrap_or_else(|error| panic!("{url}: {status}: body is not JSON: {error}"));
    assert!(!status.is_success(), "{url} succeeded: {body}");
    let code = serde_json::from_value(body["data"].clone())
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

/// Runs a Bridge with the configuration file at `config` (which may be missing) and the given
/// `RUST_LOG`, and waits for it to exit. Panics if it is still running after the timeout.
pub fn run_until_exit(config: &Path, rust_log: &str) -> Exited {
    let (mut child, output) = spawn(config, rust_log);
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

/// Starts the Bridge in the config file's directory and forwards its output lines until it
/// closes stdout and stderr (exits).
fn spawn(config: &Path, rust_log: &str) -> (Child, Receiver<Value>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_bridge"))
        .arg("--config")
        .arg(config)
        .current_dir(config.parent().expect("config file in a directory"))
        .env("RUST_LOG", rust_log)
        .env_remove("BRIDGE_CONFIG")
        // Dioxus reads static files from here; a developer's setting must not change the logs.
        .env_remove("DIOXUS_PUBLIC_PATH")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bridge");
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
