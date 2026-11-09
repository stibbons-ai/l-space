//! Calling a player's UPnP services: a SOAP request to a fixed control path, the answer read up
//! to [`MAX_BODY`] bytes and parsed for the action's output arguments. Read actions send no
//! arguments of their own (the instance id of AVTransport and GroupRenderingControl aside);
//! the actions that control playback or volume send theirs XML-escaped.

use std::fmt::Write;
use std::net::SocketAddr;
use std::time::Duration;

use reqwest::Client;
use reqwest::header::CONTENT_TYPE;

use crate::RAW;
use crate::xml::{self, Malformed};

/// The most bytes read of a player's answer; a longer one is refused. Larger than the
/// topology of a household with dozens of players.
pub const MAX_BODY: usize = 1024 * 1024;

/// A Sonos UPnP service, at the control path every player serves it on.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Service {
    ZoneGroupTopology,
    AvTransport,
    GroupRenderingControl,
}

impl Service {
    fn path(self) -> &'static str {
        match self {
            Self::ZoneGroupTopology => "/ZoneGroupTopology/Control",
            Self::AvTransport => "/MediaRenderer/AVTransport/Control",
            Self::GroupRenderingControl => "/MediaRenderer/GroupRenderingControl/Control",
        }
    }

    fn urn(self) -> &'static str {
        match self {
            Self::ZoneGroupTopology => "urn:schemas-upnp-org:service:ZoneGroupTopology:1",
            Self::AvTransport => "urn:schemas-upnp-org:service:AVTransport:1",
            Self::GroupRenderingControl => "urn:schemas-upnp-org:service:GroupRenderingControl:1",
        }
    }

    /// The input arguments every action this crate calls on the service starts with.
    fn arguments(self) -> &'static str {
        match self {
            Self::ZoneGroupTopology => "",
            Self::AvTransport | Self::GroupRenderingControl => "<InstanceID>0</InstanceID>",
        }
    }
}

/// Why a call got no usable answer.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CallError {
    /// Request construction or connection failed before a request could reach the player.
    #[error("{0}")]
    NotSent(String),
    #[error("{0}")]
    Http(String),
    /// The HTTP status, and the UPnP `errorCode` of a SOAP fault if it has a numeric one.
    #[error("HTTP status {status}{}", upnp_error.map_or_else(String::new, |code| format!(", UPnP error {code}")))]
    Status {
        status: u16,
        upnp_error: Option<u32>,
    },
    #[error("answer longer than {MAX_BODY} bytes")]
    TooLong,
    #[error(transparent)]
    Malformed(#[from] Malformed),
}

impl From<reqwest::Error> for CallError {
    fn from(error: reqwest::Error) -> Self {
        let not_sent = error.is_connect() || error.is_builder();
        let mut chain = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            chain.push_str(": ");
            chain.push_str(&cause.to_string());
            source = cause.source();
        }
        if not_sent {
            Self::NotSent(chain)
        } else {
            Self::Http(chain)
        }
    }
}

/// Calls `action` on `service` of the player at `player` and returns its output arguments.
/// It may take as long as `http` allows.
pub(crate) async fn call(
    http: &Client,
    player: SocketAddr,
    service: Service,
    action: &str,
) -> Result<Vec<(String, String)>, CallError> {
    send(http, player, service, action, &[], None).await
}

/// [`call`] with the input `arguments` (name, unescaped value) after the service's own, taking
/// at most `timeout` instead of what `http` allows.
pub(crate) async fn call_with(
    http: &Client,
    player: SocketAddr,
    service: Service,
    action: &str,
    arguments: &[(&str, &str)],
    timeout: Duration,
) -> Result<Vec<(String, String)>, CallError> {
    send(http, player, service, action, arguments, Some(timeout)).await
}

async fn send(
    http: &Client,
    player: SocketAddr,
    service: Service,
    action: &str,
    arguments: &[(&str, &str)],
    timeout: Option<Duration>,
) -> Result<Vec<(String, String)>, CallError> {
    let url = format!("http://{player}{}", service.path());
    let urn = service.urn();
    let mut inputs = service.arguments().to_owned();
    for (name, value) in arguments {
        write!(inputs, "<{name}>{}</{name}>", xml::escape(value))
            .expect("writing to a String cannot fail");
    }
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="{urn}">{inputs}</u:{action}></s:Body></s:Envelope>"#
    );
    tracing::debug!(target: RAW, peer = url, action, body, "SOAP request");
    let mut request = http
        .post(&url)
        .header(CONTENT_TYPE, r#"text/xml; charset="utf-8""#)
        .header("SOAPACTION", format!("\"{urn}#{action}\""))
        .body(body);
    if let Some(timeout) = timeout {
        request = request.timeout(timeout);
    }
    let mut response = request.send().await?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > MAX_BODY {
            tracing::debug!(target: RAW, peer = url, action, status = status.as_u16(), "SOAP response too long, dropped");
            return Err(CallError::TooLong);
        }
        bytes.extend_from_slice(&chunk);
    }
    let body = String::from_utf8_lossy(&bytes);
    tracing::debug!(target: RAW, peer = url, action, status = status.as_u16(), body = %body, "SOAP response");
    if !status.is_success() {
        return Err(CallError::Status {
            status: status.as_u16(),
            upnp_error: xml::upnp_error_code(&body),
        });
    }
    Ok(xml::soap_response(&body, action)?)
}

/// The value of output argument `name`, if the answer has it.
pub(crate) fn argument<'a>(arguments: &'a [(String, String)], name: &str) -> Option<&'a str> {
    arguments
        .iter()
        .find(|(argument, _)| argument == name)
        .map(|(_, value)| value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_refused_connection_cannot_have_sent_a_control() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let player = listener.local_addr().unwrap();
        drop(listener);
        let client = Client::builder().no_proxy().build().unwrap();
        let error = call_with(
            &client,
            player,
            Service::AvTransport,
            "Seek",
            &[("Unit", "TRACK_NR"), ("Target", "1")],
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, CallError::NotSent(_)), "{error:?}");
    }
}
