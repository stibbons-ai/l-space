//! The App API: the contract between the App and the Bridge.
//!
//! The contract is shared Rust types plus Dioxus server functions, so neither side generates
//! code from the other. With the `server` feature (the Bridge) a server function runs its body
//! and [`server::router`] serves it over HTTP; without it (the App) calling the same function
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
    /// The App could not reach the Bridge, or could not understand its answer.
    #[error("bridge_unreachable")]
    BridgeUnreachable,
    /// The Bridge failed in a way the user cannot fix.
    #[error("internal")]
    Internal,
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
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
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
