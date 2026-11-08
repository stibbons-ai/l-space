//! The test-only clock hook (`[testing] clock_address`): the Bridge times Handoff supervision
//! on a `sonos-fake` household's clock, served over HTTP, instead of the system clock, so a
//! black-box test that advances its `clock::ManualClock` moves the Bridge's Progress cadence
//! and its end-of-Item checks with the household's playback positions.
//!
//! The Bridge asks `GET /clock` once at startup (failing to start without an answer), then
//! keeps one `GET /clock?after=<what it last got>` waiting, and moves a local
//! [`ManualClock`] forward by whatever the household answers. Off unless configured; never
//! for production.

use std::net::SocketAddr;
use std::time::Duration;

use clock::ManualClock;

/// How long the first `GET /clock` may take at startup.
const FIRST_READ: Duration = Duration::from_secs(5);

/// How long a waiting `GET /clock?after=…` may take: the household answers within 10 seconds
/// even when its clock stands still.
const POLL_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the follower waits, in real time, before asking again after a failure.
const RETRY: Duration = Duration::from_millis(200);

/// Why the test clock can't be followed.
#[derive(Debug, thiserror::Error)]
#[error("cannot read the test clock at {address}: {reason}")]
pub struct Error {
    address: SocketAddr,
    reason: String,
}

/// A clock that follows the household's at `address`, as read at startup and moved on by a
/// task of its own from then on. Must be called on a Tokio runtime.
pub async fn follow(address: SocketAddr) -> Result<ManualClock, Error> {
    let http = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|error| Error {
            address,
            reason: error.to_string(),
        })?;
    let url = format!("http://{address}/clock");
    let first = tokio::time::timeout(FIRST_READ, read(&http, &url))
        .await
        .unwrap_or_else(|_| Err("no answer in time".to_owned()))
        .map_err(|reason| Error { address, reason })?;
    let clock = ManualClock::new();
    clock.advance(Duration::from_nanos(first));
    tokio::spawn(follow_from(http, url, clock.clone(), first));
    Ok(clock)
}

/// Moves `clock` on with the household's, which stood at `seen` nanoseconds.
async fn follow_from(http: reqwest::Client, url: String, clock: ManualClock, mut seen: u64) {
    loop {
        let polled = format!("{url}?after={seen}");
        match tokio::time::timeout(POLL_TIMEOUT, read(&http, &polled)).await {
            Ok(Ok(now)) => {
                if now > seen {
                    clock.advance(Duration::from_nanos(now - seen));
                    seen = now;
                }
            }
            Ok(Err(reason)) => {
                tracing::warn!(reason, "cannot follow the test clock");
                tokio::time::sleep(RETRY).await;
            }
            Err(_) => {
                tracing::warn!(reason = "no answer in time", "cannot follow the test clock");
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// The nanoseconds `url` answers with.
async fn read(http: &reqwest::Client, url: &str) -> Result<u64, String> {
    let response = http
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("status {}", response.status()));
    }
    let text = response.text().await.map_err(|error| error.to_string())?;
    text.trim()
        .parse()
        .map_err(|_| "not a number of nanoseconds".to_owned())
}
