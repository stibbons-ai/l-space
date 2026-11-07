//! How the Bridge stops when the operator asks it to: SIGTERM (`docker stop`) or SIGINT
//! (Ctrl-C). Requests in flight finish, for a bounded time, and the Bridge exits 0.

use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use test_support::bridge::{Exited, Signal, TestBridge, library_server_toml};

/// How long the Bridge lets requests in flight finish once asked to stop. Mirrors `DRAIN` in
/// `crates/bridge/src/shutdown.rs`.
const DRAIN: Duration = Duration::from_secs(5);

/// How long the Bridge waits after the drain for work still running. Mirrors
/// `RUNTIME_SHUTDOWN` in `crates/bridge/src/shutdown.rs`.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(2);

/// Room for a slow machine on top of an expected stop time.
const MARGIN: Duration = Duration::from_secs(3);

/// A Library Server that answers nothing by itself: the test takes each request and decides
/// whether, and when, to answer it.
#[derive(Debug)]
struct HeldLibraryServer {
    listener: TcpListener,
    url: String,
}

impl HeldLibraryServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("local address"));
        Self { listener, url }
    }

    /// The next request's connection, once its head has arrived. Requests have no body.
    async fn next_request(&self) -> TcpStream {
        let (mut connection, _) = self.listener.accept().await.expect("accept");
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut chunk = [0; 1024];
            let read = connection.read(&mut chunk).await.expect("read request");
            assert_ne!(read, 0, "connection closed mid-request: {head:?}");
            head.extend_from_slice(&chunk[..read]);
        }
        connection
    }
}

/// Answers a held `GET /api/me`: the key acts for user `id`.
async fn answer_me(mut connection: TcpStream, id: &str) {
    let body = serde_json::json!({"id": id, "username": "held"}).to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    );
    connection
        .write_all(response.as_bytes())
        .await
        .expect("answer");
}

fn count(exited: &Exited, message: &str) -> usize {
    exited
        .events
        .iter()
        .filter(|event| event["message"] == message)
        .count()
}

fn is_shutting_down(event: &Value) -> bool {
    event["message"] == "shutting down"
}

/// Exit 0, with one `shutting down` and one `stopped` event.
fn assert_stopped_cleanly(exited: &Exited) {
    assert!(exited.status.success(), "{exited:#?}");
    assert_eq!(count(exited, "shutting down"), 1, "{:#?}", exited.events);
    assert_eq!(count(exited, "stopped"), 1, "{:#?}", exited.events);
}

#[test]
fn sigint_stops_the_bridge_at_once_with_exit_code_0() {
    let mut bridge = TestBridge::start("");

    let asked = Instant::now();
    bridge.signal(Signal::Interrupt);
    let exited = bridge.wait_for_exit();

    assert_stopped_cleanly(&exited);
    assert!(asked.elapsed() < MARGIN, "took {:?}", asked.elapsed());
}

#[tokio::test]
async fn a_request_in_flight_at_sigterm_completes_and_its_write_lasts() {
    let library_server = HeldLibraryServer::start().await;
    let mut bridge = TestBridge::start(&library_server_toml(&library_server.url));
    let api = bridge.api().clone();
    let url = library_server.url.clone();
    let login = tokio::spawn(async move { api.login(&url, "held-key").await });
    let held = library_server.next_request().await;

    let asked = Instant::now();
    bridge.signal(Signal::Terminate);
    bridge.wait_for_event(is_shutting_down);
    answer_me(held, "held-user").await;
    let session = login.await.expect("login answered in full");
    let exited = bridge.wait_for_exit();

    assert_eq!(session.user.user_id, "held-user");
    assert_stopped_cleanly(&exited);
    assert!(asked.elapsed() < MARGIN, "took {:?}", asked.elapsed());
    // The session the login opened while the Bridge was stopping was stored: after a restart,
    // logging out finds it (and needs no Library Server).
    bridge.restart();
    bridge.api().logout(&session).await;
}

#[tokio::test]
async fn a_request_held_past_the_drain_does_not_keep_the_bridge_running() {
    let library_server = HeldLibraryServer::start().await;
    let mut bridge = TestBridge::start(&library_server_toml(&library_server.url));
    let api = bridge.api().clone();
    let url = library_server.url.clone();
    // Never answered: the Bridge's request to the Library Server waits for the ABS client's
    // 10 s `TIMEOUT` (`crates/abs/src/lib.rs`), longer than the drain allows.
    let _login = tokio::spawn(async move { api.login_error(&url, "held-key").await });
    let _held = library_server.next_request().await;

    let asked = Instant::now();
    bridge.signal(Signal::Terminate);
    bridge.wait_for_event(is_shutting_down);
    assert_refuses_connections(bridge.url());
    // A second signal during the drain has no effect: still one `shutting down`, exit 0, and
    // the full drain.
    bridge.signal(Signal::Interrupt);
    let exited = bridge.wait_for_exit();

    assert_stopped_cleanly(&exited);
    assert_eq!(
        count(
            &exited,
            "connections still open after the drain: dropping them"
        ),
        1,
        "{:#?}",
        exited.events
    );
    let took = asked.elapsed();
    assert!(
        took >= DRAIN,
        "stopped after {took:?}, before the drain ended"
    );
    assert!(took < DRAIN + MARGIN, "took {took:?}");
}

/// App API handlers run on runtimes of their own (Dioxus's), which stopping the Bridge's
/// runtime doesn't wait for: the store's close does. Here a login's write is still waiting for
/// a lock when the drain ends; the Bridge waits for it to commit before it exits.
#[tokio::test]
async fn a_write_still_running_when_the_drain_ends_commits_before_the_bridge_exits() {
    let library_server = HeldLibraryServer::start().await;
    let mut bridge = TestBridge::start(&library_server_toml(&library_server.url));
    // Another connection reading the database keeps the login's commit waiting (SQLite's
    // busy timeout is 5 s) until the test lets go.
    let reader = rusqlite::Connection::open(bridge.database_path()).expect("open the database");
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM sessions;")
        .expect("hold a read lock");
    let api = bridge.api().clone();
    let url = library_server.url.clone();
    // Its connection is dropped at the end of the drain: no answer to wait for.
    let _login = tokio::spawn(async move { api.login(&url, "held-key").await });
    let held = library_server.next_request().await;

    let asked = Instant::now();
    bridge.signal(Signal::Terminate);
    bridge.wait_for_event(is_shutting_down);
    tokio::time::sleep_until((asked + DRAIN - Duration::from_millis(600)).into()).await;
    answer_me(held, "held-user").await;
    let release_at = asked + DRAIN + Duration::from_millis(300);
    let releasing = std::thread::spawn(move || {
        std::thread::sleep(release_at.saturating_duration_since(Instant::now()));
        reader
            .execute_batch("ROLLBACK;")
            .expect("release the read lock");
    });
    let exited = bridge.wait_for_exit();
    let took = asked.elapsed();
    releasing.join().expect("lock released");

    assert_stopped_cleanly(&exited);
    assert_eq!(
        exited.events.last().map(|event| &event["message"]),
        Some(&Value::from("stopped")),
        "{:#?}",
        exited.events
    );
    assert!(took < DRAIN + RUNTIME_SHUTDOWN + MARGIN, "took {took:?}");
    let files: Vec<String> = bridge
        .database_files()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(files, ["bridge.db"], "a transaction was cut off");
    let sessions: i64 = rusqlite::Connection::open(bridge.database_path())
        .expect("reopen the database")
        .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
        .expect("count sessions");
    assert_eq!(sessions, 1, "the login's session was not stored");
}

/// The Bridge at `url` no longer accepts connections, at the latest shortly after it logged
/// `shutting down`.
fn assert_refuses_connections(url: &str) {
    let addr = url.strip_prefix("http://").expect("an http URL");
    let deadline = Instant::now() + Duration::from_secs(1);
    while std::net::TcpStream::connect(addr).is_ok() {
        assert!(
            Instant::now() < deadline,
            "{addr} still accepts connections while draining"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
