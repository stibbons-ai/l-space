//! The clock the Bridge times Handoff supervision on: the system clock, unless the test-only
//! clock hook (`[testing] clock_address`) names a `sonos-fake` household's clock, served over
//! HTTP. Then a black-box test that advances its `clock::ManualClock` moves the Bridge's
//! Progress cadence and its end-of-Item checks with the household's playback positions.
//!
//! Following, the Bridge asks `GET /clock` once at startup (failing to start without an
//! answer), then keeps one `GET /clock?after=<what it last got>` waiting, and moves a local
//! [`ManualClock`] forward by whatever the household answers. Never for production.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use clock::{Clock, ManualClock, SystemClock};

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

/// The system clock without `test_clock`; with it, a clock that follows the household's at
/// that address, as read now and moved on by a task of its own from then on (logged as a
/// warning). Must be called on a Tokio runtime.
pub async fn clock(test_clock: Option<SocketAddr>) -> Result<Arc<dyn Clock>, Error> {
    let Some(address) = test_clock else {
        return Ok(Arc::new(SystemClock));
    };
    tracing::warn!(
        %address,
        "following a test clock for Handoff supervision: a test-only hook, never for production"
    );
    let failed = |reason: String| Error { address, reason };
    let http = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|error| failed(error.to_string()))?;
    let url = format!("http://{address}/clock");
    let first = read(&http, &url, FIRST_READ).await.map_err(failed)?;
    let clock = ManualClock::new();
    clock.advance(Duration::from_nanos(first));
    tokio::spawn(follow(http, url, clock.clone(), first));
    Ok(Arc::new(clock))
}

/// Moves `clock` on with the household's, which stood at `seen` nanoseconds.
async fn follow(http: reqwest::Client, url: String, clock: ManualClock, mut seen: u64) {
    loop {
        match read(&http, &format!("{url}?after={seen}"), POLL_TIMEOUT).await {
            Ok(now) => {
                if now > seen {
                    clock.advance(Duration::from_nanos(now - seen));
                    seen = now;
                }
            }
            Err(reason) => {
                tracing::warn!(reason, "cannot follow the test clock");
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// The nanoseconds `url` answers with within `timeout`.
async fn read(http: &reqwest::Client, url: &str, timeout: Duration) -> Result<u64, String> {
    let response = http
        .get(url)
        .timeout(timeout)
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
