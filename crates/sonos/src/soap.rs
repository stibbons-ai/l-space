//! Calling a player's UPnP services: a SOAP request to a fixed control path, the answer read up
//! to [`MAX_BODY`] bytes and parsed for the action's output arguments. Only argument-less
//! read actions (plus AVTransport's instance id) are sent.

use reqwest::Client;
use reqwest::header::CONTENT_TYPE;

use crate::RAW;
use crate::xml::{self, Malformed};

/// Larger than the topology of a household with dozens of players.
const MAX_BODY: usize = 1024 * 1024;

/// A Sonos UPnP service, at the control path every player serves it on.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Service {
    ZoneGroupTopology,
    AvTransport,
}

impl Service {
    fn path(self) -> &'static str {
        match self {
            Self::ZoneGroupTopology => "/ZoneGroupTopology/Control",
            Self::AvTransport => "/MediaRenderer/AVTransport/Control",
        }
    }

    fn urn(self) -> &'static str {
        match self {
            Self::ZoneGroupTopology => "urn:schemas-upnp-org:service:ZoneGroupTopology:1",
            Self::AvTransport => "urn:schemas-upnp-org:service:AVTransport:1",
        }
    }

    /// The input arguments of every action this crate calls on the service.
    fn arguments(self) -> &'static str {
        match self {
            Self::ZoneGroupTopology => "",
            Self::AvTransport => "<InstanceID>0</InstanceID>",
        }
    }
}

/// Why a call got no usable answer.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CallError {
    #[error("{0}")]
    Http(String),
    #[error("HTTP status {0}")]
    Status(u16),
    #[error("answer longer than {MAX_BODY} bytes")]
    TooLong,
    #[error(transparent)]
    Malformed(#[from] Malformed),
}

impl From<reqwest::Error> for CallError {
    fn from(error: reqwest::Error) -> Self {
        let mut chain = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            chain.push_str(": ");
            chain.push_str(&cause.to_string());
            source = cause.source();
        }
        Self::Http(chain)
    }
}

/// Calls `action` on `service` of the player at `base_url` and returns its output arguments.
pub(crate) async fn call(
    http: &Client,
    base_url: &str,
    service: Service,
    action: &str,
) -> Result<Vec<(String, String)>, CallError> {
    let url = format!("{base_url}{}", service.path());
    let (urn, arguments) = (service.urn(), service.arguments());
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="{urn}">{arguments}</u:{action}></s:Body></s:Envelope>"#
    );
    tracing::debug!(target: RAW, peer = url, action, body, "SOAP request");
    let mut response = http
        .post(&url)
        .header(CONTENT_TYPE, r#"text/xml; charset="utf-8""#)
        .header("SOAPACTION", format!("\"{urn}#{action}\""))
        .body(body)
        .send()
        .await?;
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
        return Err(CallError::Status(status.as_u16()));
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
