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
//! - [`TestBridge::start`] writes the configuration to a temporary file, adding a `listen`
//!   address on loopback with port 0, so tests can run in parallel without port clashes.
//! - Readiness comes from the Bridge itself: it logs a `listening` event with the address it
//!   bound, and `start` waits for that event. No sleeps.
//! - Dropping the [`TestBridge`] kills the process.
//! - [`run_until_exit`] is for a Bridge that must not start: it returns the exit status and
//!   every output line, stdout and stderr. JSON lines (log events) are objects; any other line
//!   becomes a [`Value::String`].
//!
//! The Bridge's output is echoed to the test's output, so a failing test shows the Bridge's logs.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

/// Generous: the Bridge is up within milliseconds, but CI machines can be slow.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The `listen` line for a free loopback port, so tests can run in parallel.
pub const LISTEN: &str = "listen = \"127.0.0.1:0\"\n";

/// The `RUST_LOG` the harness sets unless a test asks for another, so the developer's own
/// setting cannot hide the events the harness waits for.
pub const RUST_LOG: &str = "info";

/// A running Bridge, killed on drop.
pub struct TestBridge {
    child: Child,
    api: AppApi,
    startup: Vec<Value>,
    _dir: TempDir,
}

impl TestBridge {
    /// Starts a Bridge on a free loopback port with `config` (TOML, without `listen`) and waits
    /// until it serves. Panics if it exits or does not get ready in time.
    pub fn start(config: &str) -> Self {
        Self::start_logging(config, RUST_LOG)
    }

    /// [`TestBridge::start`] with the given `RUST_LOG`, which must let the `bridge` target's
    /// `listening` event through.
    pub fn start_logging(config: &str, rust_log: &str) -> Self {
        let (dir, path) = config_file(&format!("{LISTEN}{config}"));
        let (mut child, output) = spawn(&path, rust_log);

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
                    return Self {
                        child,
                        api,
                        startup,
                        _dir: dir,
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

/// Whether `event` is the Bridge announcing it serves the App API.
pub fn is_listening(event: &Value) -> bool {
    event["message"] == "listening"
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

    /// GETs `path` and decodes the successful response body, typed or as raw JSON.
    pub async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        self.http
            .get(format!("{}{path}", self.base_url))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .unwrap_or_else(|error| panic!("GET {path}: {error}"))
            .json()
            .await
            .unwrap_or_else(|error| panic!("decode GET {path}: {error}"))
    }
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
        self.events.iter().any(|event| match event {
            Value::String(line) => line.contains(text),
            event => event.to_string().contains(text),
        })
    }
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

/// Starts the Bridge and forwards its output lines until it closes stdout and stderr (exits).
fn spawn(config: &Path, rust_log: &str) -> (Child, Receiver<Value>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_bridge"))
        .arg("--config")
        .arg(config)
        .env("RUST_LOG", rust_log)
        .env_remove("BRIDGE_CONFIG")
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
