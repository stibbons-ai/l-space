//! Logging in with an Audiobookshelf API key, the session it opens, and how that session ends
//! (ADR 0011), against a real Audiobookshelf.

use std::net::TcpListener;
use std::sync::Arc;

use api::{ApiError, CurrentUser};
use reqwest::StatusCode;
use serde_json::json;

use crate::audiobookshelf::{AbsUser, Audiobookshelf};
use crate::support::{TestBridge, bearer, output_contains};

/// A Bridge with the shared Audiobookshelf as its one Library Server.
fn bridge_with_abs() -> (TestBridge, Arc<Audiobookshelf>) {
    let abs = Audiobookshelf::shared();
    let bridge = TestBridge::start(&format!(
        "[[library_servers]]\nurl = \"{}\"\nname = \"Home\"\n",
        abs.base_url()
    ));
    (bridge, abs)
}

fn acting_as(abs: &Audiobookshelf, user: &AbsUser) -> CurrentUser {
    CurrentUser {
        library_server: abs.base_url().to_owned(),
        user_id: user.id.clone(),
        username: user.username.clone(),
    }
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
    assert_eq!(
        bridge
            .api()
            .get_error("/api/me", Some(&bearer(&first)))
            .await,
        KEY_REJECTED
    );
    // ...and every session opened with the key has ended.
    assert_eq!(
        bridge
            .api()
            .get_error("/api/me", Some(&bearer(&first)))
            .await,
        SESSION_INVALID
    );
    assert_eq!(
        bridge
            .api()
            .get_error("/api/me", Some(&bearer(&second)))
            .await,
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
async fn logging_in_with_a_new_key_ends_the_sessions_of_the_old_one() {
    let (bridge, abs) = bridge_with_abs();
    let alice = abs.new_user().await;
    let old = bridge.api().login(abs.base_url(), &alice.api_key.key).await;

    let new_key = abs.new_api_key(&alice.id).await;
    let new = bridge.api().login(abs.base_url(), &new_key.key).await;

    assert_eq!(
        bridge.api().get_error("/api/me", Some(&bearer(&old))).await,
        SESSION_INVALID
    );
    assert_eq!(
        bridge.api().current_user(&new).await,
        acting_as(&abs, &alice)
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
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let bridge = TestBridge::start(&format!("[[library_servers]]\nurl = \"http://{closed}\"\n"));

    assert_eq!(
        bridge
            .api()
            .login_error(&format!("http://{closed}"), "any-key")
            .await,
        (StatusCode::BAD_GATEWAY, ApiError::LibraryServerUnreachable)
    );
}

#[tokio::test]
async fn sessions_survive_a_bridge_restart() {
    let (mut bridge, abs) = bridge_with_abs();
    let alice = abs.new_user().await;
    let session = bridge.api().login(abs.base_url(), &alice.api_key.key).await;

    bridge.restart();

    assert_eq!(
        bridge.api().current_user(&session).await,
        acting_as(&abs, &alice)
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
        bridge
            .api()
            .get_error("/api/me", Some(&bearer(&phone)))
            .await,
        SESSION_INVALID
    );
    assert_eq!(
        bridge
            .api()
            .post_error("/api/logout", Some(&bearer(&phone)), json!({}))
            .await,
        SESSION_INVALID
    );
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
    let bridge = TestBridge::start_logging(
        &format!("[[library_servers]]\nurl = \"{}\"\n", abs.base_url()),
        "trace",
    );
    let alice = abs.new_user().await;
    let session = bridge.api().login(abs.base_url(), &alice.api_key.key).await;
    bridge.api().current_user(&session).await;
    abs.delete_api_key(&alice.api_key).await;
    bridge
        .api()
        .get_error("/api/me", Some(&bearer(&session)))
        .await;

    let output = bridge.stop();
    for secret in [alice.api_key.key.as_str(), session.token.expose()] {
        assert!(
            !output_contains(&output, secret),
            "a secret leaked into the logs"
        );
    }
}
