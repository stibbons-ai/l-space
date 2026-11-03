//! The App API: the contract between the App and the Bridge.
//!
//! The contract is shared Rust types plus Dioxus server functions, so neither side generates
//! code from the other. With the `server` feature (the Bridge) a server function runs its body
//! and `server::router` serves it over HTTP; without it (the App) calling the same function
//! sends the HTTP request to the Bridge.
//!
//! Errors the App shows to a person are [`ApiError`] codes, which the App translates. The
//! Bridge never sends prose for the App to display.

use dioxus::fullstack::{AsStatusCode, ServerFnError, StatusCode};
#[cfg(feature = "server")]
use dioxus::prelude::dioxus_server;
use dioxus::prelude::{dioxus_fullstack, get};
use serde::{Deserialize, Serialize};

/// A Library Server the operator configured on the Bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryServer {
    /// The normalized base URL, which identifies the Library Server (ADR 0006).
    pub base_url: String,
    /// The operator's display name for it, if any.
    pub name: Option<String>,
}

/// An error code the App translates for the user.
///
/// On the wire this is the `data` member of the error body, for example
/// `{"code":"internal"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ApiError {
    /// The App could not reach the Bridge, or could not understand its answer. Produced only
    /// on the App's side, by the client half of a server function; the Bridge never sends it.
    #[error("bridge_unreachable")]
    BridgeUnreachable,
    /// The Bridge failed in a way the user cannot fix.
    #[error("internal")]
    Internal,
    /// A code this build of the App does not know yet, from a newer Bridge.
    #[serde(other)]
    #[error("unknown")]
    Unknown,
}

impl From<ServerFnError> for ApiError {
    fn from(error: ServerFnError) -> Self {
        match error {
            ServerFnError::Request(_)
            | ServerFnError::StreamError(_)
            | ServerFnError::Deserialization(_) => Self::BridgeUnreachable,
            _ => Self::Internal,
        }
    }
}

impl AsStatusCode for ApiError {
    fn as_status_code(&self) -> StatusCode {
        match self {
            Self::BridgeUnreachable => StatusCode::BAD_GATEWAY,
            Self::Internal | Self::Unknown => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// Lists the Library Servers configured on the Bridge, in configuration order.
#[get("/api/library-servers", bridge: dioxus_fullstack::extract::Extension<server::BridgeHandle>)]
pub async fn library_servers() -> Result<Vec<LibraryServer>, ApiError> {
    Ok(bridge.library_servers())
}

/// The Bridge's side of the App API.
#[cfg(feature = "server")]
pub mod server {
    use std::sync::Arc;

    use dioxus::fullstack::axum::{Extension, Router};
    use dioxus::server::{DioxusRouterExt, FullstackState};

    use crate::LibraryServer;

    /// What the App API needs from the Bridge. The `bridge` binary implements it.
    pub trait Bridge: Send + Sync + 'static {
        /// The configured Library Servers, in configuration order.
        fn library_servers(&self) -> Vec<LibraryServer>;
    }

    /// How server functions reach the [`Bridge`].
    pub type BridgeHandle = Arc<dyn Bridge>;

    /// An axum router serving every App API server function, backed by `bridge`.
    pub fn router(bridge: BridgeHandle) -> Router {
        Router::new()
            .register_server_functions()
            .layer(Extension(bridge))
            .with_state(FullstackState::headless())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::ApiError;

    #[test]
    fn error_codes_travel_as_a_code_field() {
        assert_eq!(
            serde_json::to_value(ApiError::Internal).unwrap(),
            json!({"code": "internal"})
        );
        assert_eq!(
            serde_json::from_value::<ApiError>(json!({"code": "internal"})).unwrap(),
            ApiError::Internal
        );
    }

    #[test]
    fn a_code_from_a_newer_bridge_decodes_as_unknown() {
        assert_eq!(
            serde_json::from_value::<ApiError>(json!({"code": "api_key_rejected"})).unwrap(),
            ApiError::Unknown
        );
    }
}
