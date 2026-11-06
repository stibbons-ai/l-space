//! The HTTP client every server function call goes through, and the one request the App sends
//! past it: following an entered address's redirects.
//!
//! Dioxus fullstack's own client follows redirects, so a call could land on another origin than
//! the stored Bridge address: a `POST` redirected with 301 or 302 turns into a `GET`, and reqwest
//! drops `Authorization` on the way to another scheme, host or port, so the session looks ended.
//! The App's client follows none: a call reaches the stored address only, and a redirect answer
//! is an error the App shows (`not_a_bridge`, or `bridge_unreachable` once the address is stored).

use std::sync::{Arc, LazyLock, Once};

use dioxus::fullstack::GLOBAL_REQUEST_CLIENT;
use reqwest_fullstack::Client;
use reqwest_fullstack::redirect::Policy;
use url::Url;

/// How many redirects [`redirect_target`] follows, as reqwest does by default.
const MAX_REDIRECTS: usize = 10;

/// Sends every server function call through a client that follows no redirect. Must run before
/// the first call: fullstack otherwise installs its own client for the rest of the process.
pub(crate) fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        if GLOBAL_REQUEST_CLIENT.set(client(Policy::none())).is_err() {
            tracing::error!(
                "a server function call ran before the App's client: calls follow redirects"
            );
        }
    });
}

/// Where a `GET` of `url` ends up after following its redirects, if it is redirected and the
/// last hop answers. `None` for no redirect, too many, or a hop that fails. Sends none of the
/// server function calls' headers, so no session token.
pub(crate) async fn redirect_target(url: Url) -> Option<Url> {
    static FOLLOWING: LazyLock<Client> = LazyLock::new(|| client(Policy::limited(MAX_REDIRECTS)));
    let response = FOLLOWING
        .get(url.clone())
        .send()
        .await
        .inspect_err(|error| tracing::debug!(%error, "following the address's redirects failed"))
        .ok()?;
    (response.url() != &url).then(|| response.url().clone())
}

/// A client set up like fullstack's (`ClientRequest::new_reqwest_client`), redirects as `policy`
/// says. Without fullstack's cookie store: the Bridge authenticates by header only.
fn client(policy: Policy) -> Client {
    Client::builder()
        .redirect(policy)
        .tls_backend_preconfigured(tls_config())
        .build()
        .expect("a client with a preconfigured rustls backend builds")
}

/// Fullstack's TLS config (its `rustls_client_config`): _ring_ with the bundled webpki roots, so
/// it works without platform verifier setup (which Android requires).
fn tls_config() -> rustls::ClientConfig {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default TLS versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    // As fullstack's: its websocket upgrades require HTTP/1.1.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config
}
