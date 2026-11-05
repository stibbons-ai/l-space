//! The Bridge's media streams (ADR 0010): `/stream/<stream id>/<file>`, on the App API's
//! listener, which Groups fetch over the LAN. A stream id names one active Handoff and serves
//! only its Item's audio file, forwarding byte ranges to the Library Server with the starter's
//! API key. Speakers can't authenticate, so the id is the only capability: 256 random bits,
//! stored only as its hash, never logged. Anything that doesn't name an active Handoff's file
//! is a bare `404`, alike for an unknown id, a known one with another file name, and a path
//! that isn't UTF-8.

use std::fmt::Write;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::StreamExt;
use store::Store;

use crate::app_api::AppApi;

/// The router serving every stream, for `app`'s Handoffs. `GET` routes answer `HEAD` too.
pub fn router<S: Store>(app: Arc<AppApi<S>>) -> Router {
    Router::new()
        .route("/stream/{id}/{file}", get(serve::<S>))
        .with_state(app)
}

/// A stream's id as it appears in its URL: the capability that lets a Group fetch the stream.
/// It never shows in `Debug` output; only its hash, [`store::StreamId`], is stored.
pub struct StreamToken(String);

impl StreamToken {
    pub fn new(token: String) -> Self {
        Self(token)
    }

    /// What the store knows the stream by.
    pub fn id(&self) -> store::StreamId {
        store::StreamId::of(&self.0)
    }

    /// The path of this stream serving the file named `file_name`, percent-encoded.
    pub fn path(&self, file_name: &str) -> String {
        format!("/stream/{}/{}", self.0, path_segment(file_name))
    }
}

impl std::fmt::Debug for StreamToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StreamToken(REDACTED)")
    }
}

/// `text` as one URL path segment: ASCII letters, digits and `-._~` as they are, every other
/// byte percent-encoded.
fn path_segment(text: &str) -> String {
    let mut segment = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            segment.push(char::from(byte));
        } else {
            write!(segment, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    segment
}

/// The `Range` a request asks for, if it is one to forward: a single `bytes` range, as
/// speakers send, its first byte not after its last. Anything else is ignored, and the whole
/// file served (RFC 9110, 14.2).
fn range(headers: &HeaderMap) -> Option<&str> {
    let range = headers.get(RANGE)?.to_str().ok()?;
    let spec = range.strip_prefix("bytes=")?;
    let (first, last) = spec.split_once('-')?;
    // A part is absent (`None`), or a byte position of at most 20 digits; else no range.
    let number = |part: &str| -> Option<Option<u64>> {
        if part.is_empty() {
            return Some(None);
        }
        let digits = part.len() <= 20 && part.bytes().all(|byte| byte.is_ascii_digit());
        digits.then(|| part.parse().ok()).flatten().map(Some)
    };
    match (number(first)?, number(last)?) {
        (None, None) => None,
        (Some(first), Some(last)) if first > last => None,
        _ => Some(range),
    }
}

/// Serves a stream's file, or the part of it asked for.
async fn serve<S: Store>(
    State(app): State<Arc<AppApi<S>>>,
    path: Result<Path<(String, String)>, PathRejection>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    // A path that isn't UTF-8 names nothing either.
    let Ok(Path((id, file))) = path else {
        return not_found();
    };
    let (library_server, store::ActiveHandoff { handoff, api_key }) =
        match app.active_handoff(&id, &file).await {
            Ok(Some(active)) => active,
            Ok(None) => return not_found(),
            Err(error) => {
                crate::app_api::log_storage_error("look up a stream", &error);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
    let Some(content_type) = sonos::media_type(&handoff.file.name) else {
        tracing::warn!(
            group = handoff.group,
            library_item = handoff.library_item,
            "a Handoff's file is in a format Sonos doesn't play"
        );
        return not_found();
    };
    let request = if method == Method::HEAD {
        abs::FileRequest::Head
    } else {
        abs::FileRequest::Get
    };
    let range = range(&headers);
    let answer = app
        .abs
        .file(
            library_server,
            &api_key,
            &handoff.library_item,
            &handoff.file.ino,
            request,
            range,
        )
        .await;
    let file = match answer {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(
                group = handoff.group,
                library_server,
                user_id = handoff.started_by.user_id,
                %error,
                "cannot stream a Handoff's file from its Library Server"
            );
            return match error {
                abs::Error::NotFound(_) => not_found(),
                abs::Error::Rejected | abs::Error::Unavailable(_) => {
                    StatusCode::BAD_GATEWAY.into_response()
                }
            };
        }
    };
    tracing::debug!(
        group = handoff.group,
        status = file.status,
        range,
        "streaming a Handoff's file"
    );
    let status = StatusCode::from_u16(file.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = Response::builder()
        .status(status)
        .header(ACCEPT_RANGES, "bytes");
    if status != StatusCode::RANGE_NOT_SATISFIABLE {
        response = response.header(CONTENT_TYPE, content_type);
    }
    if let Some(length) = file.content_length {
        response = response.header(CONTENT_LENGTH, length);
    }
    if let Some(content_range) = file
        .content_range
        .and_then(|range| HeaderValue::from_str(&range).ok())
    {
        response = response.header(CONTENT_RANGE, content_range);
    }
    let body = if request == abs::FileRequest::Head {
        Body::empty()
    } else {
        let group = handoff.group;
        Body::from_stream(file.body.map(move |chunk| {
            chunk.map_err(|error| {
                tracing::warn!(
                    group,
                    %error,
                    "a Handoff's file broke off while streaming from its Library Server"
                );
                axum::Error::new(error)
            })
        }))
    };
    response
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// The answer to whatever names no active Handoff's file: the same for every such request.
fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::{StreamToken, range};

    #[test]
    fn a_file_name_is_one_percent_encoded_segment() {
        let token = StreamToken::new("ab12".into());
        assert_eq!(
            token.path("Episode 1/../ä?#%.mp3"),
            "/stream/ab12/Episode%201%2F..%2F%C3%A4%3F%23%25.mp3"
        );
        assert_eq!(token.path("a-b_c~d.mp3"), "/stream/ab12/a-b_c~d.mp3");
        assert_eq!(format!("{token:?}"), "StreamToken(REDACTED)");
    }

    #[test]
    fn only_a_single_byte_range_is_forwarded() {
        let range_of = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("range", HeaderValue::from_str(value).unwrap());
            range(&headers).map(str::to_owned)
        };
        for forwarded in ["bytes=0-", "bytes=100-199", "bytes=-500", "bytes=5-5"] {
            assert_eq!(range_of(forwarded).as_deref(), Some(forwarded));
        }
        for ignored in [
            "bytes=-",
            "bytes=0-1,5-6",
            "items=0-1",
            "bytes=a-b",
            "bytes= 0-1",
            "bytes=000000000000000000000-",
            "bytes=5-1",
            "bytes=99999999999999999999-",
        ] {
            assert_eq!(range_of(ignored), None, "{ignored:?}");
        }
        assert_eq!(range(&HeaderMap::new()), None);
    }
}
