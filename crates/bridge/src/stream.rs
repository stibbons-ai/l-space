//! The Bridge's media streams (ADR 0010): `/stream/<stream id>/<file>`, on the App API's
//! listener, which Groups fetch over the LAN and the App plays Local playback from. A stream
//! id names one active Handoff or Local session and serves only its Item's audio file,
//! forwarding byte ranges to the Library Server with the API key it acts with (a Handoff's
//! starter's, a Local session's session's). Speakers and the phone's player don't
//! authenticate, so the id is the only capability: 256 random bits, stored only as its hash,
//! never logged at INFO or above (the SOAP bodies at DEBUG under `sonos::raw` carry the URL).
//! Anything that doesn't name an active stream's file is a bare `404`, alike for an unknown
//! id, a known one with another file name, and a path that isn't UTF-8. A Local session's
//! Item's cover is served the same way at `/cover/<stream id>`.

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

/// The router serving every stream, for `app`'s Handoffs and Local sessions, and Local
/// sessions' covers. `GET` routes answer `HEAD` too.
pub fn router<S: Store>(app: Arc<AppApi<S>>) -> Router {
    Router::new()
        .route("/stream/{id}/{file}", get(serve::<S>))
        .route("/cover/{id}", get(serve_cover::<S>))
        .with_state(app)
}

/// A stream's id as it appears in its URL: the capability that lets a Group fetch the stream.
/// It never shows in `Debug` output; only its hash, [`store::StreamId`], is stored.
pub struct StreamToken(String);

impl StreamToken {
    pub fn new(token: String) -> Self {
        Self(token)
    }

    /// What the store knows the stream by: the id's hash.
    pub fn hash(&self) -> store::StreamId {
        store::StreamId::of(&self.0)
    }

    /// The path of this stream serving the file named `file_name`, percent-encoded.
    pub fn path(&self, file_name: &str) -> String {
        format!("/stream/{}/{}", self.0, path_segment(file_name))
    }

    /// The path of the cover of this stream's Item: Local sessions' only.
    pub fn cover_path(&self) -> String {
        format!("/cover/{}", self.0)
    }
}

impl std::fmt::Debug for StreamToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StreamToken(REDACTED)")
    }
}

/// The stream named in `url`, if it is a stream's URL as a Group holds it:
/// `http://<host>/stream/<id>/<file>`, on whatever address and port. Only the shape is
/// checked; whether it names a Handoff is the store's to say.
pub fn stream_in_url(url: &str) -> Option<store::StreamId> {
    let host_and_path = url.strip_prefix("http://")?;
    stream_in_path(&host_and_path[host_and_path.find('/')?..])
}

/// The stream named in `path`, if it is a stream's path as [`StreamToken::path`] makes it:
/// `/stream/<id>/<file>`. Only the shape is checked, as for [`stream_in_url`].
pub fn stream_in_path(path: &str) -> Option<store::StreamId> {
    let mut segments = path.strip_prefix('/')?.split('/');
    let (stream, id, file) = (segments.next()?, segments.next()?, segments.next()?);
    let shaped = stream == "stream" && !id.is_empty() && !file.is_empty();
    (shaped && segments.next().is_none()).then(|| store::StreamId::of(id))
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

/// What a stream serves: an active Handoff's or Local session's file, found by its stream id
/// and file name.
struct Source<'a> {
    library_server: &'a str,
    api_key: abs::ApiKey,
    library_item: String,
    file: store::HandoffFile,
    /// Who acts for it: a Handoff's starter, a Local session's user.
    user_id: String,
    /// For the log: the Handoff's Group; none for a Local session.
    group: Option<String>,
    /// For the log: "a Handoff" or "a Local session".
    what: &'static str,
}

/// The source of the stream `id`, if it names an active Handoff or Local session and `file`
/// is its file's name; none otherwise, alike for every reason.
async fn source<'a, S: Store>(
    app: &'a AppApi<S>,
    id: &store::StreamId,
    file: &str,
) -> Result<Option<Source<'a>>, store::Error> {
    if let Some((library_server, store::ActiveHandoff { handoff, api_key })) =
        app.active_handoff(id, file).await?
    {
        return Ok(Some(Source {
            library_server,
            api_key,
            library_item: handoff.library_item,
            file: handoff.file,
            user_id: handoff.started_by.user_id,
            group: Some(handoff.group),
            what: "a Handoff",
        }));
    }
    let Some((library_server, active)) = app.active_local(id).await? else {
        return Ok(None);
    };
    if file != active.local.file.name {
        return Ok(None);
    }
    Ok(Some(Source {
        library_server,
        api_key: active.api_key,
        library_item: active.local.library_item,
        file: active.local.file,
        user_id: active.user.user_id,
        group: None,
        what: "a Local session",
    }))
}

/// Serves a stream's file, or the part of it asked for.
async fn serve<S: Store>(
    State(app): State<Arc<AppApi<S>>>,
    path: Result<Path<(String, String)>, PathRejection>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    // A path that isn't UTF-8 names nothing either.
    let (id, file) = match path {
        Ok(Path(path)) => path,
        Err(rejection) => {
            tracing::debug!(%rejection, "a stream path that names nothing");
            return not_found();
        }
    };
    let source = match source(&app, &StreamToken::new(id).hash(), &file).await {
        Ok(Some(source)) => source,
        Ok(None) => return not_found(),
        Err(error) => {
            crate::app_api::log_storage_error("look up a stream", &error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let Source {
        library_server,
        api_key,
        library_item,
        file: streamed,
        user_id,
        group,
        what,
    } = source;
    let Some(content_type) = sonos::media_type(&streamed.name) else {
        tracing::warn!(
            group,
            library_item,
            "{what}'s file is in a format Sonos doesn't play"
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
            &library_item,
            &streamed.ino,
            request,
            range,
        )
        .await;
    let file = match answer {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(
                group,
                library_server,
                user_id,
                %error,
                "cannot stream {what}'s file from its Library Server"
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
        group,
        status = file.status,
        range,
        "streaming {what}'s file"
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
        let library_server = library_server.to_owned();
        let mut sent: u64 = 0;
        Body::from_stream(file.body.map(move |chunk| match chunk {
            Ok(chunk) => {
                sent += chunk.len() as u64;
                Ok(chunk)
            }
            Err(error) => {
                tracing::warn!(
                    group,
                    library_server,
                    sent,
                    %error,
                    "{what}'s file broke off while streaming from its Library Server"
                );
                Err(axum::Error::new(error))
            }
        }))
    };
    response
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Serves the cover of a Local session's Item: `/cover/<stream id>`, alive as long as the
/// Local session's stream. Anything else is a bare `404`, as for a stream.
async fn serve_cover<S: Store>(
    State(app): State<Arc<AppApi<S>>>,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let Ok(Path(id)) = path else {
        return not_found();
    };
    let (library_server, active) = match app.active_local(&StreamToken::new(id).hash()).await {
        Ok(Some(active)) => active,
        Ok(None) => return not_found(),
        Err(error) => {
            crate::app_api::log_storage_error("look up a cover", &error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let cover = app
        .abs
        .cover(library_server, &active.api_key, &active.local.library_item)
        .await;
    match cover {
        Ok(cover) => ([(CONTENT_TYPE, cover.content_type)], cover.bytes).into_response(),
        Err(abs::Error::NotFound(_)) => not_found(),
        Err(error) => {
            tracing::warn!(
                library_server,
                user_id = active.user.user_id,
                %error,
                "cannot fetch a Local session's cover from its Library Server"
            );
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// The answer to whatever names no active Handoff's or Local session's file, or no active
/// Local session's cover; to an active Handoff's or Local session's file, or a Local
/// session's cover, that the Library Server says doesn't exist (most often an Item without a
/// cover); and to an active Handoff's or Local session's file in a format Sonos doesn't play:
/// the same for every such request.
fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::{StreamToken, range, stream_in_path, stream_in_url};

    #[test]
    fn a_streams_id_is_read_back_from_its_url_on_any_address() {
        let token = StreamToken::new("ab12".into());
        let path = token.path("Episode 1.mp3");
        for url in [
            format!("http://127.0.0.1:9{path}"),
            format!("http://[::1]:8080{path}"),
        ] {
            assert_eq!(stream_in_url(&url), Some(token.hash()), "{url}");
        }
        for other in [
            "http://127.0.0.1:9/stream/ab12",
            "http://127.0.0.1:9/stream//x.mp3",
            "http://127.0.0.1:9/stream/ab12/x.mp3/more",
            "http://127.0.0.1:9/track/ab12/x.mp3",
            "https://127.0.0.1:9/stream/ab12/x.mp3",
            "x-sonos-spotify:spotify%3atrack%3a1",
            "http://127.0.0.1:9",
            "",
        ] {
            assert_eq!(stream_in_url(other), None, "{other}");
        }
        // As the App names a Local session's stream: by its path.
        assert_eq!(stream_in_path(&path), Some(token.hash()));
        for other in ["stream/ab12/x.mp3", "/cover/ab12", "/stream/ab12", ""] {
            assert_eq!(stream_in_path(other), None, "{other}");
        }
    }

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
