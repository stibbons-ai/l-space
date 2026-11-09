//! The harness the Bridge's black-box tests and the App's flow tests share: a real Bridge
//! process ([`bridge`]) and a real Audiobookshelf in a container ([`audiobookshelf`]).

use std::time::{Duration, Instant};

pub mod audiobookshelf;
pub mod bridge;

/// How long [`eventually`] waits by default, in real time.
pub const WITHIN: Duration = Duration::from_secs(10);

/// A loopback address nothing listens on.
pub fn closed_port() -> std::net::SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("bind a loopback port")
}

/// Waits until `done`, checking every 10 ms in real time, at most `within`; panics with `what`
/// if it never is.
pub async fn eventually(what: &str, within: Duration, mut done: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !done().await {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
