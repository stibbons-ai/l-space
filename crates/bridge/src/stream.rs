//! The Bridge's media streams (ADR 0010): `/stream/<stream id>/<file>`, on the App API's
//! listener, which Groups fetch over the LAN. A stream id names one active Handoff and serves
//! only its Item's audio file, forwarding byte ranges to the Library Server with the starter's
//! API key. Speakers can't authenticate, so the id is the only capability: 256 random bits,
//! stored only as its hash. Anything that doesn't name an active Handoff's file is `404`,
//! alike for an unknown id and a known one with another file name.

use std::fmt::Write;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
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

/// The path of the stream `id` serving the file named `file_name`, percent-encoded.
pub fn path(id: &str, file_name: &str) -> String {
    format!("/stream/{id}/{}", path_segment(file_name))
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
/// speakers send. Anything else is ignored, and the whole file served (RFC 9110, 14.2).
fn range(headers: &HeaderMap) -> Option<&str> {
    let range = headers.get(RANGE)?.to_str().ok()?;
    let spec = range.strip_prefix("bytes=")?;
    let (first, last) = spec.split_once('-')?;
    let digits = |part: &str| part.len() <= 20 && part.bytes().all(|byte| byte.is_ascii_digit());
    let single = digits(first) && digits(last) && !(first.is_empty() && last.is_empty());
    single.then_some(range)
}

/// Serves a stream's file, or the part of it asked for.
async fn serve<S: Store>(
    State(app): State<Arc<AppApi<S>>>,
    Path((id, file)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
) -> Response {
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
        Body::from_stream(file.body.map(|chunk| chunk.map_err(axum::Error::new)))
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

    use super::{path, range};

    #[test]
    fn a_file_name_is_one_percent_encoded_segment() {
        assert_eq!(
            path("ab12", "Episode 1/../ä?#%.mp3"),
            "/stream/ab12/Episode%201%2F..%2F%C3%A4%3F%23%25.mp3"
        );
        assert_eq!(path("ab12", "a-b_c~d.mp3"), "/stream/ab12/a-b_c~d.mp3");
    }

    #[test]
    fn only_a_single_byte_range_is_forwarded() {
        let range_of = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("range", HeaderValue::from_str(value).unwrap());
            range(&headers).map(str::to_owned)
        };
        for forwarded in ["bytes=0-", "bytes=100-199", "bytes=-500"] {
            assert_eq!(range_of(forwarded).as_deref(), Some(forwarded));
        }
        for ignored in [
            "bytes=-",
            "bytes=0-1,5-6",
            "items=0-1",
            "bytes=a-b",
            "bytes= 0-1",
            "bytes=000000000000000000000-",
        ] {
            assert_eq!(range_of(ignored), None, "{ignored:?}");
        }
        assert_eq!(range(&HeaderMap::new()), None);
    }
}
