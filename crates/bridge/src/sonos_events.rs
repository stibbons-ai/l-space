//! GENA callbacks share the Bridge's fixed HTTP listener. Input is bounded before parsing;
//! only current subscription SIDs and forward sequences can wake the state reconciler.

use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::routing::any;
use sonos::Sonos;

pub fn router(sonos: Arc<dyn Sonos>) -> Router {
    Router::new()
        .route("/sonos/events", any(notify))
        .layer(DefaultBodyLimit::max(sonos::MAX_BODY))
        .with_state(sonos)
}

async fn notify(
    State(sonos): State<Arc<dyn Sonos>>,
    method: Method,
    headers: HeaderMap,
    body: String,
) -> StatusCode {
    if method.as_str() != "NOTIFY" {
        return StatusCode::METHOD_NOT_ALLOWED;
    }
    let header = |name| headers.get(name).and_then(|value| value.to_str().ok());
    if header("nt") != Some("upnp:event") || header("nts") != Some("upnp:propchange") {
        return StatusCode::BAD_REQUEST;
    }
    let (Some(sid), Some(seq)) = (
        header("sid"),
        header("seq").and_then(|seq| seq.parse().ok()),
    ) else {
        return StatusCode::BAD_REQUEST;
    };
    match sonos.notify(sid, seq, &body) {
        sonos::Notification::Accepted | sonos::Notification::Ignored => StatusCode::OK,
        sonos::Notification::Malformed => StatusCode::BAD_REQUEST,
        sonos::Notification::UnknownSubscription => StatusCode::PRECONDITION_FAILED,
    }
}
