//! An HTTP proxy on loopback in front of a Library Server that, once told to, answers every
//! Item lookup (`GET /api/items/<id>`, exactly one path segment after `/api/items/`, any query)
//! with each `duration` in it set to `null`: an Item whose length Audiobookshelf doesn't know
//! (as for an audio file it could not probe), without seeding one. Everything else, also every
//! deeper path under an Item (its files, its cover), passes as it is.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::header;
use axum::response::Response;
use tokio::task::JoinHandle;

/// The most of a body the proxy passes on.
const MAX_BODY: usize = 64 << 20;

/// Proxies requests on its own port to a Library Server; stops when dropped.
pub struct LengthHider {
    port: u16,
    hiding: Arc<AtomicBool>,
    serving: JoinHandle<()>,
}

#[derive(Clone)]
struct Proxy {
    target: String,
    hiding: Arc<AtomicBool>,
    http: reqwest::Client,
}

impl LengthHider {
    /// Starts proxying a free loopback port to `target`, a base URL; hides no length yet.
    pub async fn to(target: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind length hider");
        let port = listener.local_addr().expect("length hider address").port();
        let hiding = Arc::new(AtomicBool::new(false));
        let proxy = Proxy {
            target: target.trim_end_matches('/').to_owned(),
            hiding: Arc::clone(&hiding),
            http: reqwest::Client::new(),
        };
        let router = Router::new().fallback(forward).with_state(proxy);
        let serving = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("length hider");
        });
        Self {
            port,
            hiding,
            serving,
        }
    }

    /// The proxy's URL, as an operator would configure it.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// From now on, Item lookups tell no length.
    pub fn hide(&self) {
        self.hiding.store(true, Ordering::SeqCst);
    }
}

impl Drop for LengthHider {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

async fn forward(State(proxy): State<Proxy>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map_or("/", |path| path.as_str())
        .to_owned();
    let hidden = proxy.hiding.load(Ordering::SeqCst)
        && parts.method == axum::http::Method::GET
        && parts
            .uri
            .path()
            .strip_prefix("/api/items/")
            .is_some_and(|id| !id.is_empty() && !id.contains('/'));
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
