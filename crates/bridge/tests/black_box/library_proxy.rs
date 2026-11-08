//! An HTTP proxy on loopback in front of a Library Server that, once told to, answers some
//! requests itself instead of passing them on:
//!
//! - [`LibraryProxy::hide_lengths`]: every Item lookup (`GET /api/items/<id>`, exactly one path
//!   segment after `/api/items/`, any query) with each `duration` in it set to `null`: an Item
//!   whose length Audiobookshelf doesn't know (as for an audio file it could not probe),
//!   without seeding one.
//! - [`LibraryProxy::reject_item_lookups`]: every Item lookup at once with 401, as for a
//!   rejected key.
//! - [`LibraryProxy::hang_identity`]: every `GET /api/me` never.
//! - [`LibraryProxy::reject_progress_writes`]: every `PATCH /api/me/progress/...` at once with
//!   401, as for a rejected key, while the key stays valid, until
//!   [`LibraryProxy::pass_progress_writes`].
//! - [`LibraryProxy::fail_progress_writes`]: the next few of them with 500, as a Library
//!   Server failing for a moment would.
//! - [`LibraryProxy::delay_progress_writes`]: every one of them only after a while, as a slow
//!   Library Server would.
//!
//! Everything else, also every deeper path under an Item (its files, its cover), passes as it
//! is. [`LibraryProxy::progress_writes`] counts the Progress writes that arrived, whatever
//! the proxy answered them, and [`LibraryProxy::identity_requests`] every `GET /api/me`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::response::Response;
use tokio::task::JoinHandle;

/// The most of a body the proxy passes on.
const MAX_BODY: usize = 64 << 20;

/// Proxies requests on its own port to a Library Server; stops when dropped.
pub struct LibraryProxy {
    port: u16,
    faults: Arc<Faults>,
    serving: JoinHandle<()>,
}

/// What the proxy answers itself; none at first.
#[derive(Default)]
struct Faults {
    hide_lengths: AtomicBool,
    reject_item_lookups: AtomicBool,
    hang_identity: AtomicBool,
    reject_progress_writes: AtomicBool,
    /// How many Progress writes arrived so far.
    progress_writes: AtomicUsize,
    /// How many Progress writes are still to fail.
    fail_progress_writes: AtomicUsize,
    /// How many failed so far.
    progress_writes_failed: AtomicUsize,
    /// How long, in milliseconds, each Progress write is held before it is passed on.
    progress_write_delay_ms: AtomicU64,
    /// How many `GET /api/me` arrived so far.
    identity_requests: AtomicUsize,
}

#[derive(Clone)]
struct Proxy {
    target: String,
    faults: Arc<Faults>,
    http: reqwest::Client,
}

impl LibraryProxy {
    /// Starts proxying a free loopback port to `target`, a base URL; answers nothing itself yet.
    pub async fn to(target: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind Library Server proxy");
        let port = listener.local_addr().expect("proxy address").port();
        let faults = Arc::new(Faults::default());
        let proxy = Proxy {
            target: target.trim_end_matches('/').to_owned(),
            faults: Arc::clone(&faults),
            http: reqwest::Client::new(),
        };
        let router = Router::new().fallback(forward).with_state(proxy);
        let serving = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("Library Server proxy");
        });
        Self {
            port,
            faults,
            serving,
        }
    }

    /// The proxy's URL, as an operator would configure it.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// From now on, Item lookups tell no length.
    pub fn hide_lengths(&self) {
        self.faults.hide_lengths.store(true, Ordering::SeqCst);
    }

    /// From now on, Item lookups are rejected with 401 at once.
    pub fn reject_item_lookups(&self) {
        self.faults
            .reject_item_lookups
            .store(true, Ordering::SeqCst);
    }

    /// From now on, `GET /api/me` is never answered.
    pub fn hang_identity(&self) {
        self.faults.hang_identity.store(true, Ordering::SeqCst);
    }

    /// From now on, Progress writes are rejected with 401 at once.
    pub fn reject_progress_writes(&self) {
        self.faults
            .reject_progress_writes
            .store(true, Ordering::SeqCst);
    }

    /// From now on, Progress writes pass again.
    pub fn pass_progress_writes(&self) {
        self.faults
            .reject_progress_writes
            .store(false, Ordering::SeqCst);
    }

    /// The next `times` Progress writes fail with 500, without reaching the Library Server.
    pub fn fail_progress_writes(&self, times: usize) {
        self.faults
            .fail_progress_writes
            .store(times, Ordering::SeqCst);
    }

    /// How many Progress writes failed with 500 so far ([`Self::fail_progress_writes`]).
    pub fn progress_writes_failed(&self) -> usize {
        self.faults.progress_writes_failed.load(Ordering::SeqCst)
    }

    /// How many Progress writes arrived so far, answered by the proxy or passed on.
    pub fn progress_writes(&self) -> usize {
        self.faults.progress_writes.load(Ordering::SeqCst)
    }

    /// How many `GET /api/me` (whom a key acts for) arrived so far, answered or not.
    pub fn identity_requests(&self) -> usize {
        self.faults.identity_requests.load(Ordering::SeqCst)
    }

    /// From now on, each Progress write is passed on only after `delay`, in real time.
    pub fn delay_progress_writes(&self, delay: Duration) {
        let millis = u64::try_from(delay.as_millis()).expect("a delay in milliseconds");
        self.faults
            .progress_write_delay_ms
            .store(millis, Ordering::SeqCst);
    }
}

impl Drop for LibraryProxy {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

async fn forward(State(proxy): State<Proxy>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let faults = &proxy.faults;
    let path = parts
        .uri
        .path_and_query()
        .map_or("/", |path| path.as_str())
        .to_owned();
    let item_lookup = parts.method == Method::GET
        && parts
            .uri
            .path()
            .strip_prefix("/api/items/")
            .is_some_and(|id| !id.is_empty() && !id.contains('/'));
    let progress_write =
        parts.method == Method::PATCH && parts.uri.path().starts_with("/api/me/progress/");
    if progress_write {
        faults.progress_writes.fetch_add(1, Ordering::SeqCst);
    }
    let rejected = (item_lookup && faults.reject_item_lookups.load(Ordering::SeqCst))
        || (progress_write && faults.reject_progress_writes.load(Ordering::SeqCst));
    if rejected {
        let response = Response::builder().status(StatusCode::UNAUTHORIZED);
        return response.body(Body::empty()).expect("response");
    }
    let failing = |left: usize| left.checked_sub(1);
    if progress_write
        && faults
            .fail_progress_writes
            .try_update(Ordering::SeqCst, Ordering::SeqCst, failing)
            .is_ok()
    {
        faults.progress_writes_failed.fetch_add(1, Ordering::SeqCst);
        let response = Response::builder().status(StatusCode::INTERNAL_SERVER_ERROR);
        return response.body(Body::empty()).expect("response");
    }
    if progress_write {
        let delay = faults.progress_write_delay_ms.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    if parts.method == Method::GET && parts.uri.path() == "/api/me" {
        faults.identity_requests.fetch_add(1, Ordering::SeqCst);
        if faults.hang_identity.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
    }
    let hidden = item_lookup && faults.hide_lengths.load(Ordering::SeqCst);
    let body = to_bytes(body, MAX_BODY).await.expect("request body");
    let mut sent = proxy
        .http
        .request(parts.method, format!("{}{path}", proxy.target))
        .body(body);
    for (name, value) in &parts.headers {
        if ![
            header::HOST,
            header::CONTENT_LENGTH,
            header::CONNECTION,
            header::ACCEPT_ENCODING,
        ]
        .contains(name)
        {
            sent = sent.header(name, value);
        }
    }
    let answer = sent.send().await.expect("Library Server answers");
    let status = answer.status();
    let headers = answer.headers().clone();
    let mut bytes = answer.bytes().await.expect("answer body").to_vec();
    if hidden && status.is_success() {
        let mut json: serde_json::Value = serde_json::from_slice(&bytes).expect("Item JSON");
        hide_durations(&mut json);
        bytes = serde_json::to_vec(&json).expect("Item JSON");
    }
    let mut response = Response::builder().status(status);
    for (name, value) in &headers {
        if ![
            header::CONTENT_LENGTH,
            header::TRANSFER_ENCODING,
            header::CONNECTION,
        ]
        .contains(name)
        {
            response = response.header(name, value);
        }
    }
    response.body(Body::from(bytes)).expect("response")
}

/// Sets every `duration` in `json` to `null`.
fn hide_durations(json: &mut serde_json::Value) {
    match json {
        serde_json::Value::Object(object) => {
            if let Some(duration) = object.get_mut("duration") {
                *duration = serde_json::Value::Null;
            }
            object.values_mut().for_each(hide_durations);
        }
        serde_json::Value::Array(array) => array.iter_mut().for_each(hide_durations),
        _ => {}
    }
}
