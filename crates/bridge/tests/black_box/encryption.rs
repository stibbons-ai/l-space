//! Stored API keys are encrypted under the operator's encryption key: given from a file or the
//! environment, never readable from the database files, and a different key ends the sessions
//! cleanly instead of decrypting them.

use api::{ApiError, CurrentUser};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::StatusCode;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::audiobookshelf::{AbsUser, Audiobookshelf};
use crate::support::{KEY, KeyEnv, KeyFile, OTHER_KEY, TestBridge, library_server_toml};

const SESSION_INVALID: (StatusCode, ApiError) =
    (StatusCode::UNAUTHORIZED, ApiError::SessionInvalid);

fn acting_as(abs: &Audiobookshelf, user: &AbsUser) -> CurrentUser {
    CurrentUser {
        library_server: abs.base_url().to_owned(),
        user_id: user.id.clone(),
        username: user.username.clone(),
    }
}

fn events<'a>(output: &'a [Value], message: &'a str) -> impl Iterator<Item = &'a Value> {
    output
        .iter()
        .filter(move |event| event["message"] == message)
}

const OTHER_KEY_WARNING: &str = "stored API keys were encrypted with another encryption key; \
                                 their sessions end at their next call, and their users must \
                                 log in again";

#[tokio::test]
async fn a_key_file_and_the_same_key_in_the_environment_open_the_same_sessions() {
    let abs = Audiobookshelf::shared();
    let mut bridge = TestBridge::start_with_key(
        &library_server_toml(abs.base_url()),
        KeyEnv::file(&format!("{KEY}\n")),
    );
    let alice = abs.new_user().await;
    let session = bridge.api().login(abs.base_url(), &alice.api_key.key).await;

    bridge.restart_with_key(KeyEnv::value(KEY));
    assert_eq!(
        bridge.api().current_user(&session).await,
        acting_as(&abs, &alice)
    );

    bridge.restart_with_key(KeyEnv::file(KEY));
    assert_eq!(
        bridge.api().current_user(&session).await,
        acting_as(&abs, &alice)
    );
}

/// With both variables set, the file's key is the one in use, and the warning names both
/// variables but neither key.
#[tokio::test]
async fn the_key_file_wins_over_the_environment() {
    let abs = Audiobookshelf::shared();
    let mut bridge = TestBridge::start_with_key(
        &library_server_toml(abs.base_url()),
        KeyEnv {
            file: Some(KeyFile::Holding(KEY.into())),
            value: Some(OTHER_KEY.into()),
        },
    );
    let warnings: Vec<_> = bridge
        .startup_output()
        .iter()
        .filter(|event| event["level"] == "WARN")
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:#?}");
    let warning = warnings[0]["message"].as_str().expect("message");
    assert!(
        warning.contains("BRIDGE_ENCRYPTION_KEY_FILE")
            && warning.contains("BRIDGE_ENCRYPTION_KEY "),
        "{warning}"
    );
    let alice = abs.new_user().await;
    let session = bridge.api().login(abs.base_url(), &alice.api_key.key).await;

    bridge.restart_with_key(KeyEnv::value(KEY));
    assert_eq!(
        bridge.api().current_user(&session).await,
        acting_as(&abs, &alice)
    );

    let output = bridge.stop();
    for key in [KEY, OTHER_KEY] {
        assert!(
            !crate::support::output_contains(&output, key),
            "an encryption key leaked"
        );
    }
}

/// Neither the database file nor its side files hold the API key, or its plain SHA-256 that
/// would let a guessed key be confirmed.
#[tokio::test]
async fn the_database_files_hold_no_plain_api_key() {
    let abs = Audiobookshelf::shared();
    let bridge = TestBridge::start(&library_server_toml(abs.base_url()));
    let alice = abs.new_user().await;
    bridge.api().login(abs.base_url(), &alice.api_key.key).await;

    let key = alice.api_key.key.as_bytes();
    let sha256 = Sha256::digest(key);
    let hex: String = sha256.iter().map(|byte| format!("{byte:02x}")).collect();
    let forbidden: [(&str, Vec<u8>); 5] = [
        ("the API key", key.to_vec()),
        ("its SHA-256", sha256.to_vec()),
        ("its SHA-256 in hex", hex.clone().into_bytes()),
        (
            "its SHA-256 in upper-case hex",
            hex.to_uppercase().into_bytes(),
        ),
        (
            "its SHA-256 in base64",
            STANDARD.encode(sha256).into_bytes(),
        ),
    ];
    let files = bridge.database_files();
    // Control: the file read is the one the login went to.
    assert!(
        files
            .iter()
            .any(|(_, bytes)| contains(bytes, alice.id.as_bytes())),
        "no database file holds the user id: {:?}",
        files.iter().map(|(name, _)| name).collect::<Vec<_>>()
    );
    for (name, bytes) in &files {
        for (what, needle) in &forbidden {
            assert!(!contains(bytes, needle), "{name} holds {what}");
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[tokio::test]
async fn another_encryption_key_ends_sessions_until_a_fresh_login() {
    let abs = Audiobookshelf::shared();
    let mut bridge = TestBridge::start(&library_server_toml(abs.base_url()));
    let alice = abs.new_user().await;
    let old = bridge.api().login(abs.base_url(), &alice.api_key.key).await;
    assert_eq!(
        events(bridge.startup_output(), OTHER_KEY_WARNING).count(),
        0,
        "nothing was stored yet"
    );

    bridge.restart_with_key(KeyEnv::value(OTHER_KEY));
    assert_eq!(
        events(bridge.startup_output(), OTHER_KEY_WARNING).count(),
        1
    );
    assert_eq!(bridge.api().current_user_error(&old).await, SESSION_INVALID);
    assert_eq!(bridge.api().current_user_error(&old).await, SESSION_INVALID);

    let fresh = bridge.api().login(abs.base_url(), &alice.api_key.key).await;
    assert_eq!(
        bridge.api().current_user(&fresh).await,
        acting_as(&abs, &alice)
    );

    // The unreadable key was removed: no warning when starting with this key again.
    bridge.restart();
    assert_eq!(
        events(bridge.startup_output(), OTHER_KEY_WARNING).count(),
        0
    );
    assert_eq!(bridge.api().current_user_error(&old).await, SESSION_INVALID);
    assert_eq!(
        bridge.api().current_user(&fresh).await,
        acting_as(&abs, &alice)
    );

    // Back to the first key: the old session stays gone, and now the fresh one ends.
    bridge.restart_with_key(KeyEnv::value(KEY));
    assert_eq!(
        events(bridge.startup_output(), OTHER_KEY_WARNING).count(),
        1
    );
    assert_eq!(bridge.api().current_user_error(&old).await, SESSION_INVALID);
    assert_eq!(
        bridge.api().current_user_error(&fresh).await,
        SESSION_INVALID
    );
    let output = bridge.stop();
    let removals: Vec<_> = events(&output, KEY_REMOVED).collect();
    assert_eq!(removals.len(), 1, "{output:#?}");
    assert_eq!(removals[0]["user_id"], alice.id);
    assert_eq!(removals[0]["library_server"], abs.base_url());
}

const KEY_REMOVED: &str = "a stored API key does not decrypt with the current encryption key \
                           (or was tampered with); it was removed and the sessions opened with \
                           it ended";
