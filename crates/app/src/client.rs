//! The HTTP client every server function call goes through, and the one request the App sends
//! past it, which follows redirects: discovering where an entered address redirects to.
//!
//! Dioxus fullstack's own client follows redirects, so a call could land on another origin than
//! the stored Bridge address: a `POST` redirected with 301 or 302 turns into a `GET`, and reqwest
//! drops `Authorization` on the way to another scheme, host or port, so the session looks ended.
//! The App's client follows none: a call reaches the stored address only, and a redirect answer
//! is an error the App shows (`not_a_bridge`, or `bridge_unreachable` once the address is stored).

use std::sync::{Arc, LazyLock, Once};

use dioxus::fullstack::GLOBAL_REQUEST_CLIENT;
use dioxus::fullstack::reqwest::Client;
use dioxus::fullstack::reqwest::redirect::Policy;
use url::Url;

/// Sends every server function call through a client that follows no redirect. Must run before
/// the first call: fullstack otherwise installs its own client for the rest of the process.
pub(crate) fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        if GLOBAL_REQUEST_CLIENT.set(client(Policy::none())).is_err() {
            tracing::error!(
                "a server function call ran before the App's client: calls follow redirects"
            );
            // Loud in tests and debug builds, where an early call is a bug to fix.
            #[cfg(debug_assertions)]
            panic!("a server function call ran before `App::open`");
        }
    });
}

/// The most redirects [`redirect_target`] follows (reqwest's default).
const MAX_REDIRECTS: usize = 10;

/// The redirect target couldn't be reached: a connection, DNS or TLS failure, or no answer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Unreachable;

/// Where a `GET` of `url` ends up after following its redirects (at most [`MAX_REDIRECTS`],
/// never from `https` to `http`), if it is redirected and the last hop answers. `Ok(None)` for
/// no redirect, too many, a refused hop or another failure that isn't about reaching a hop.
/// Sends none of the server function calls' headers, so no session token.
pub(crate) async fn redirect_target(url: Url) -> Result<Option<Url>, Unreachable> {
    static FOLLOWING: LazyLock<Client> = LazyLock::new(|| {
        client(Policy::custom(|attempt| {
            let previous = attempt.previous();
            if previous.len() > MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if previous
                .last()
                .is_some_and(|previous| !may_follow(previous, attempt.url()))
            {
                attempt.error("a redirect from https to http")
            } else {
                attempt.follow()
            }
        }))
    });
    match FOLLOWING.get(url.clone()).send().await {
        Ok(response) => {
            let target = response.url();
            Ok((target != &url).then(|| target.clone()))
        }
        Err(error) if error.is_connect() || error.is_timeout() => {
            tracing::debug!(%error, "a redirect of the address can't be reached");
            Err(Unreachable)
        }
        Err(error) => {
            tracing::debug!(%error, "following the address's redirects failed");
            Ok(None)
        }
    }
}

/// Whether a redirect from `previous` to `next` may be followed: never from `https` to `http`,
/// which would move the address (and so the session token and API key) off TLS.
fn may_follow(previous: &Url, next: &Url) -> bool {
    !(previous.scheme() == "https" && next.scheme() == "http")
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

#[cfg(test)]
mod tests {
    use super::may_follow;
    use url::Url;

    #[test]
    fn a_redirect_is_followed_unless_it_leaves_https_for_http() {
        for (previous, next, expected) in [
            ("https://a.example/x", "http://a.example/x", false),
            ("https://a.example/x", "http://b.example:8080/x", false),
            ("http://a.example/x", "https://b.example/x", true),
            ("https://a.example/x", "https://b.example/x", true),
            ("http://a.example/x", "http://b.example/x", true),
        ] {
            let (previous, next) = (Url::parse(previous).unwrap(), Url::parse(next).unwrap());
            assert_eq!(
                may_follow(&previous, &next),
                expected,
                "{previous} -> {next}"
            );
        }
    }
}
