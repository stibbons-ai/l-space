//! The black-box Bridge harness.
//!
//! A test starts the real `bridge` binary as a subprocess and acts on it only through the App
//! API, the way the App would. It never reaches into the Bridge's modules or state.
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
//!   the log events, which are JSON objects, one per stdout line.
//!
//! The Bridge's stdout is echoed to the test's output, so a failing test shows the Bridge's logs.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

/// Generous: the Bridge is up within milliseconds, but CI machines can be slow.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The `listen` line for a free loopback port, so tests can run in parallel.
pub const LISTEN: &str = "listen = \"127.0.0.1:0\"\n";

/// A running Bridge, killed on drop.
pub struct TestBridge {
    child: Child,
    api: AppApi,
    _dir: TempDir,
}

impl TestBridge {
    /// Starts a Bridge on a free loopback port with `config` (TOML, without `listen`) and waits
    /// until it serves. Panics if it exits or does not get ready in time.
    pub fn start(config: &str) -> Self {
        let (dir, path) = config_file(&format!("{LISTEN}{config}"));
        let (mut child, events) = spawn(&path);

        let deadline = Instant::now() + TIMEOUT;
        loop {
            match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(event) if is_listening(&event) => {
                    let Some(addr) = event["addr"].as_str() else {
                        let _ = child.kill();
                        panic!("listening event has no addr: {event}");
                    };
                    let api = AppApi {
                        http: reqwest::Client::new(),
                        base_url: format!("http://{addr}"),
                    };
                    return Self {
                        child,
                        api,
                        _dir: dir,
                    };
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("Bridge exited before serving: {:?}", child.wait());
                }
                Err(RecvTimeoutError::Timeout) => {
                    let _ = child.kill();
                    panic!("Bridge not ready within {TIMEOUT:?}");
                }
            }
        }
    }

    /// A client for this Bridge's App API.
    pub fn api(&self) -> &AppApi {
        &self.api
    }
}

/// Whether `event` is the Bridge announcing it serves the App API.
pub fn is_listening(event: &Value) -> bool {
    event["message"] == "listening"
}

impl Drop for TestBridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
        self.http
            .get(format!("{}/api/library-servers", self.base_url))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .expect("GET /api/library-servers")
            .json()
            .await
            .expect("decode library servers")
    }
}

/// How a Bridge that was expected to stop ended.
#[derive(Debug)]
pub struct Exited {
    pub status: ExitStatus,
    /// Every log event the Bridge wrote, in order.
    pub events: Vec<Value>,
}

impl Exited {
    pub fn errors(&self) -> impl Iterator<Item = &Value> {
        self.events.iter().filter(|event| event["level"] == "ERROR")
    }
}

/// Runs a Bridge with the configuration file at `config` (which may be missing) and waits for
/// it to exit. Panics if it is still running after the timeout.
pub fn run_until_exit(config: &Path) -> Exited {
    let (mut child, receiver) = spawn(config);
    let deadline = Instant::now() + TIMEOUT;
    let mut events = Vec::new();
    loop {
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(event) => events.push(event),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                panic!("Bridge still running after {TIMEOUT:?}; events: {events:#?}");
            }
        }
    }
    let status = child.wait().expect("wait for Bridge");
    Exited { status, events }
}

/// A temporary directory holding `contents` as a config file, for [`run_until_exit`].
pub fn config_file(contents: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("bridge.toml");
    std::fs::write(&path, contents).expect("write config");
    (dir, path)
}

/// Starts the Bridge and forwards its stdout log events until it closes stdout (exits).
fn spawn(config: &Path) -> (Child, Receiver<Value>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_bridge"))
        .arg("--config")
        .arg(config)
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn bridge");
    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            eprintln!("[bridge] {line}");
            let event = serde_json::from_str(&line).unwrap_or(Value::String(line));
            // Keep draining after the receiver is gone, so the Bridge never blocks on a full pipe.
            let _ = sender.send(event);
        }
    });
    (child, receiver)
}
