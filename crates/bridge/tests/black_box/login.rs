//! Logging in with an Audiobookshelf API key, the session it opens, and how that session ends
//! (ADR 0011), against a real Audiobookshelf.

use std::net::TcpListener;
use std::sync::Arc;

use api::{ApiError, CurrentUser};
use reqwest::StatusCode;
use serde_json::json;

use crate::audiobookshelf::{AbsUser, Audiobookshelf};
use crate::forwarder::Forwarder;
use crate::support::{TestBridge, library_server_toml, output_contains};

/// The configuration naming `abs` as the one Library Server.
fn abs_config(abs: &Audiobookshelf) -> String {
    format!("{}name = \"Home\"\n", library_server_toml(abs.base_url()))
}

/// A Bridge with the shared Audiobookshelf as its one Library Server.
fn bridge_with_abs() -> (TestBridge, Arc<Audiobookshelf>) {
    let abs = Audiobookshelf::shared();
    let bridge = TestBridge::start(&abs_config(&abs));
    (bridge, abs)
}

fn acting_as(abs: &Audiobookshelf, user: &AbsUser) -> CurrentUser {
    acting_on(abs.base_url(), user)
}

fn acting_on(library_server: &str, user: &AbsUser) -> CurrentUser {
    CurrentUser {
        library_server: library_server.to_owned(),
        user_id: user.id.clone(),
        username: user.username.clone(),
    }
}

/// A loopback address nothing listens on.
fn closed_port() -> std::net::SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

const SESSION_INVALID: (StatusCode, ApiError) =
    (StatusCode::UNAUTHORIZED, ApiError::SessionInvalid);
const KEY_REJECTED: (StatusCode, ApiError) = (StatusCode::UNAUTHORIZED, ApiError::ApiKeyRejected);

#[tokio::test]
async fn a_valid_key_opens_a_session_that_acts_as_its_user() {
    let (bridge, abs) = bridge_with_abs();
    let alice = abs.new_user().await;

    let session = bridge.api().login(abs.base_url(), &alice.api_key.key).await;

    assert_eq!(session.user, acting_as(&abs, &alice));
    assert_eq!(
        bridge.api().current_user(&session).await,
        acting_as(&abs, &alice)
    );
    // Audiobookshelf agrees whose key it is.
    assert_eq!(abs.whose_key(&alice.api_key).await["id"], alice.id);
}

#[tokio::test]
async fn each_session_acts_only_as_the_user_who_opened_it() {
    let (bridge, abs) = bridge_with_abs();
    let alice = abs.new_user().await;
    let bob = abs.new_user().await;

    let alices = bridge.api().login(abs.base_url(), &alice.api_key.key).await;
    let bobs = bridge.api().login(abs.base_url(), &bob.api_key.key).await;

    assert_ne!(alices.token, bobs.token);
    assert_eq!(
        bridge.api().current_user(&alices).await,
        acting_as(&abs, &alice)
    );
    assert_eq!(
        bridge.api().current_user(&bobs).await,
        acting_as(&abs, &bob)
    );
}

#[tokio::test]
async fn any_spelling_of_a_configured_library_server_logs_in() {
    let (bridge, abs) = bridge_with_abs();
    let alice = abs.new_user().await;
    let spelled = format!("{}/", abs.base_url().replacen("http://", "HTTP://", 1));

    let session = bridge.api().login(&spelled, &alice.api_key.key).await;

    assert_eq!(session.user, acting_as(&abs, &alice));
}

/// Pins the wire shape the App decodes, independent of the shared Rust types.
#[tokio::test]
async fn session_wire_shape() {
    let (bridge, abs) = bridge_with_abs();
    let alice = abs.new_user().await;

    let response = bridge
        .api()
        .post(
            "/api/login",
            None,
            json!({"library_server": abs.base_url(), "api_key": alice.api_key.key}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body: serde_json::Value = response.json().await.unwrap();

    let token = body["token"].take();
    let token = token.as_str().expect("token is a string");
    assert_eq!(token.len(), 64, "{token}");
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()), "{token}");
    assert_eq!(
        body,
        json!({
            "token": null,
            "user": {
                "library_server": abs.base_url(),
                "user_id": alice.id,
                "username": alice.username,
            },
        })
    );
}

#[tokio::test]
async fn a_wrong_key_is_rejected() {
    let (bridge, abs) = bridge_with_abs();

    for key in ["not-a-key", "", "line\nbreak"] {
        assert_eq!(
            bridge.api().login_error(abs.base_url(), key).await,
            KEY_REJECTED,
            "{key:?}"
        );
    }
}

#[tokio::test]
async fn a_revoked_key_ends_its_sessions() {
    let (bridge, abs) = bridge_with_abs();
    let alice = abs.new_user().await;
    let bob = abs.new_user().await;
    let first = bridge.api().login(abs.base_url(), &alice.api_key.key).await;
    let second = bridge.api().login(abs.base_url(), &alice.api_key.key).await;
    let bobs = bridge.api().login(abs.base_url(), &bob.api_key.key).await;

    abs.delete_api_key(&alice.api_key).await;

    // The first call after the revocation learns it from Audiobookshelf...
    assert_eq!(bridge.api().current_user_error(&first).await, KEY_REJECTED);
    // ...and every session opened with the key has ended.
    assert_eq!(
        bridge.api().current_user_error(&first).await,
        SESSION_INVALID
    );
    assert_eq!(
        bridge.api().current_user_error(&second).await,
        SESSION_INVALID
    );
    assert_eq!(
        bridge
            .api()
            .login_error(abs.base_url(), &alice.api_key.key)
            .await,
        KEY_REJECTED
    );
    // Other users' sessions go on.
    assert_eq!(
        bridge.api().current_user(&bobs).await,
        acting_as(&abs, &bob)
    );
}

#[tokio::test]
async fn each_key_of_a_user_keeps_its_own_sessions() {
    let (bridge, abs) = bridge_with_abs();
    let alice = abs.new_user().await;
    let phone = bridge.api().login(abs.base_url(), &alice.api_key.key).await;
    let tablet_key = abs.new_api_key(&alice.id).await;
    let tablet = bridge.api().login(abs.base_url(), &tablet_key.key).await;

    assert_eq!(
        bridge.api().current_user(&phone).await,
        acting_as(&abs, &alice)
    );
    assert_eq!(
        bridge.api().current_user(&tablet).await,
        acting_as(&abs, &alice)
    );

    abs.delete_api_key(&alice.api_key).await;

    assert_eq!(bridge.api().current_user_error(&phone).await, KEY_REJECTED);
    assert_eq!(
        bridge.api().current_user_error(&phone).await,
        SESSION_INVALID
    );
    assert_eq!(
        bridge.api().current_user(&tablet).await,
        acting_as(&abs, &alice)
    );
}

#[tokio::test]
async fn a_key_moved_to_another_user_ends_its_sessions() {
    let (bridge, abs) = bridge_with_abs();
    let alice = abs.new_user().await;
    let bob = abs.new_user().await;
    let session = bridge.api().login(abs.base_url(), &alice.api_key.key).await;

    abs.move_api_key(&alice.api_key, &bob.id).await;

    assert_eq!(
        bridge.api().current_user_error(&session).await,
        KEY_REJECTED
    );
    assert_eq!(
        bridge.api().current_user_error(&session).await,
        SESSION_INVALID
    );
}

#[tokio::test]
async fn an_unconfigured_library_server_is_rejected() {
    let (bridge, abs) = bridge_with_abs();
    let alice = abs.new_user().await;

    for library_server in [
        "https://elsewhere.lan",
        &format!("{}/other-path", abs.base_url()),
        "not a url",
    ] {
        assert_eq!(
            bridge
                .api()
                .login_error(library_server, &alice.api_key.key)
                .await,
            (StatusCode::NOT_FOUND, ApiError::LibraryServerNotConfigured),
            "{library_server:?}"
        );
    }
}

#[tokio::test]
async fn an_unreachable_library_server_is_reported() {
    let closed = closed_port();
    let bridge = TestBridge::start(&library_server_toml(&format!("http://{closed}")));

    assert_eq!(
        bridge
            .api()
            .login_error(&format!("http://{closed}"), "any-key")
            .await,
        (StatusCode::BAD_GATEWAY, ApiError::LibraryServerUnreachable)
    );
}

#[tokio::test]
async fn an_outage_keeps_the_session() {
    let abs = Audiobookshelf::shared();
    let mut library_server = Forwarder::to(abs.address()).await;
    let bridge = TestBridge::start(&library_server_toml(&library_server.url()));
    let alice = abs.new_user().await;
    let session = bridge
        .api()
        .login(&library_server.url(), &alice.api_key.key)
        .await;

    library_server.stop().await;
    assert_eq!(
        bridge.api().current_user_error(&session).await,
        (StatusCode::BAD_GATEWAY, ApiError::LibraryServerUnreachable)
    );

    library_server.resume().await;
    assert_eq!(
        bridge.api().current_user(&session).await,
        acting_on(&library_server.url(), &alice)
    );
}

#[tokio::test]
async fn sessions_on_two_library_servers_stay_apart() {
    let abs = Audiobookshelf::shared();
    // The same Audiobookshelf under a second URL is a second Library Server to the Bridge.
    let office = Forwarder::to(abs.address()).await;
    let bridge = TestBridge::start(&format!(
        "{}{}",
        library_server_toml(abs.base_url()),
        library_server_toml(&office.url())
    ));
    let alice = abs.new_user().await;
    let home_session = bridge.api().login(abs.base_url(), &alice.api_key.key).await;
    let office_key = abs.new_api_key(&alice.id).await;
    let office_session = bridge.api().login(&office.url(), &office_key.key).await;

    assert_eq!(home_session.user, acting_as(&abs, &alice));
    assert_eq!(office_session.user, acting_on(&office.url(), &alice));
    assert_eq!(
        bridge.api().current_user(&home_session).await,
        acting_as(&abs, &alice)
    );

    abs.delete_api_key(&office_key).await;

    assert_eq!(
        bridge.api().current_user_error(&office_session).await,
        KEY_REJECTED
    );
    assert_eq!(
        bridge.api().current_user(&home_session).await,
        acting_as(&abs, &alice)
    );
}

#[tokio::test]
async fn sessions_and_logouts_survive_a_bridge_restart() {
    let (mut bridge, abs) = bridge_with_abs();
    let alice = abs.new_user().await;
    let kept = bridge.api().login(abs.base_url(), &alice.api_key.key).await;
    let logged_out = bridge.api().login(abs.base_url(), &alice.api_key.key).await;
    bridge.api().logout(&logged_out).await;

    bridge.restart();

    assert_eq!(
        bridge.api().current_user(&kept).await,
        acting_as(&abs, &alice)
    );
    assert_eq!(
        bridge.api().current_user_error(&logged_out).await,
        SESSION_INVALID
    );
}

#[tokio::test]
async fn removing_the_library_server_from_the_configuration_ends_its_sessions() {
    let (mut bridge, abs) = bridge_with_abs();
    let alice = abs.new_user().await;
    let session = bridge.api().login(abs.base_url(), &alice.api_key.key).await;

    bridge.restart_with("");

    assert_eq!(
        bridge.api().current_user_error(&session).await,
        SESSION_INVALID
    );
}

#[tokio::test]
async fn logout_ends_only_that_session() {
    let (bridge, abs) = bridge_with_abs();
    let alice = abs.new_user().await;
    let phone = bridge.api().login(abs.base_url(), &alice.api_key.key).await;
    let tablet = bridge.api().login(abs.base_url(), &alice.api_key.key).await;

    bridge.api().logout(&phone).await;

    assert_eq!(
        bridge.api().current_user_error(&phone).await,
        SESSION_INVALID
    );
    assert_eq!(bridge.api().logout_error(&phone).await, SESSION_INVALID);
    assert_eq!(
        bridge.api().current_user(&tablet).await,
        acting_as(&abs, &alice)
    );
}

#[tokio::test]
async fn calls_without_a_valid_session_are_rejected() {
    let bridge = TestBridge::start("");

    for authorization in [
        None,
        Some("Bearer 0000000000000000000000000000000000000000000000000000000000000000"),
        Some("Bearer garbage"),
        Some("Basic YWxpY2U6c2VjcmV0"),
        Some("Bearer"),
    ] {
        assert_eq!(
            bridge.api().get_error("/api/me", authorization).await,
            SESSION_INVALID,
            "GET /api/me with {authorization:?}"
        );
        assert_eq!(
            bridge
                .api()
                .post_error("/api/logout", authorization, json!({}))
                .await,
            SESSION_INVALID,
            "POST /api/logout with {authorization:?}"
        );
    }
}

#[tokio::test]
async fn keys_and_session_tokens_stay_out_of_the_logs() {
    let abs = Audiobookshelf::shared();
    let closed = format!("http://{}", closed_port());
    let bridge = TestBridge::start_logging(
        &format!("{}{}", abs_config(&abs), library_server_toml(&closed)),
        "trace",
    );
    let alice = abs.new_user().await;
    let wrong_key = "wrong-key-s3cret";

    bridge.api().login_error(abs.base_url(), wrong_key).await;
    bridge.api().login_error(&closed, &alice.api_key.key).await;
    let logged_out = bridge.api().login(abs.base_url(), &alice.api_key.key).await;
    bridge.api().logout(&logged_out).await;
    let session = bridge.api().login(abs.base_url(), &alice.api_key.key).await;
    bridge.api().current_user(&session).await;
    abs.delete_api_key(&alice.api_key).await;
    bridge.api().current_user_error(&session).await;

    let output = bridge.stop();
    assert!(
        output
            .iter()
            .any(|event| event["message"] == "API key revoked, its sessions ended"),
        "the revocation was not logged: {output:#?}"
    );
    for secret in [
        alice.api_key.key.as_str(),
        wrong_key,
        logged_out.token.expose(),
        session.token.expose(),
    ] {
        assert!(
            !output_contains(&output, secret),
            "a secret leaked into the logs"
        );
    }
}
