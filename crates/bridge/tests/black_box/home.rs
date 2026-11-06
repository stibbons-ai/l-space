//! Home for a session's user, against the seeded Audiobookshelf: Progress is set through
//! Audiobookshelf's own API with each user's key, as their player would, never through the
//! Bridge.

use std::time::Duration;

use api::{ApiError, Progress};
use reqwest::StatusCode;

use test_support::audiobookshelf::{AbsUser, Audiobookshelf};
use test_support::bridge::{
    KEY_REJECTED, SESSION_INVALID, TestBridge, home_episode as episode,
    home_long_book as long_book, library_server_toml, named_library_server_toml,
};

use crate::browse::logged_in;
use crate::forwarder::Forwarder;

fn progress(current_time: f64) -> Option<Progress> {
    Some(Progress {
        current_time,
        finished: false,
    })
}

/// Records `user`'s Progress in episode `n` (or, without one, in "The Long Book") a moment
/// after the last, so Audiobookshelf's update times (milliseconds) tell them apart.
async fn listen(abs: &Audiobookshelf, user: &AbsUser, n: Option<usize>, at: f64, finished: bool) {
    tokio::time::sleep(Duration::from_millis(20)).await;
    let seeded = abs.seeded();
    let (library_item, episode) = match n {
        Some(n) => (&seeded.podcast, Some(seeded.episodes[n].as_str())),
        None => (&seeded.long_book, None),
    };
    abs.set_progress(user, library_item, episode, at, finished)
        .await;
}

#[tokio::test]
async fn without_progress_home_lists_only_the_latest_episodes_newest_first() {
    let (bridge, abs, _, session) = logged_in().await;

    let home = bridge.api().home(&session).await;

    assert_eq!(home.continue_listening, []);
    assert_eq!(
        home.latest_episodes,
        [
            episode(&abs, 2, None),
            episode(&abs, 1, None),
            episode(&abs, 0, None)
        ]
    );
}

/// Audiobooks and episodes alike, the last listened to first; an Item at 0 seconds is not
/// listed, as Audiobookshelf has it. An episode in progress is in both lists.
#[tokio::test]
async fn continue_listening_lists_unfinished_items_most_recently_listened_first() {
    let (bridge, abs, user, session) = logged_in().await;
    let seeded = abs.seeded();
    listen(&abs, &user, None, 3000.0, false).await;
    listen(&abs, &user, Some(1), 300.0, false).await;
    listen(&abs, &user, Some(0), 120.0, false).await;
    abs.set_progress(&user, &seeded.silence, None, 0.0, false)
        .await;

    let home = bridge.api().home(&session).await;
    assert_eq!(
        home.continue_listening,
        [
            episode(&abs, 0, progress(120.0)),
            episode(&abs, 1, progress(300.0)),
            long_book(&abs, progress(3000.0)),
        ]
    );
    assert_eq!(
        home.latest_episodes,
        [
            episode(&abs, 2, None),
            episode(&abs, 1, progress(300.0)),
            episode(&abs, 0, progress(120.0)),
        ]
    );

    listen(&abs, &user, None, 3100.0, false).await;
    let home = bridge.api().home(&session).await;
    assert_eq!(
        home.continue_listening,
        [
            long_book(&abs, progress(3100.0)),
            episode(&abs, 0, progress(120.0)),
            episode(&abs, 1, progress(300.0)),
        ]
    );
}

#[tokio::test]
async fn finished_items_are_in_neither_list() {
    let (bridge, abs, user, session) = logged_in().await;
    listen(&abs, &user, None, 3000.0, false).await;
    listen(&abs, &user, Some(2), 100.0, false).await;

    listen(&abs, &user, None, 3000.0, true).await;
    listen(&abs, &user, Some(2), 100.0, true).await;
    listen(&abs, &user, Some(0), 0.0, true).await;

    let home = bridge.api().home(&session).await;
    assert_eq!(home.continue_listening, []);
    assert_eq!(home.latest_episodes, [episode(&abs, 1, None)]);
}

/// Another user's Progress, Finished included, neither shows in a user's Home nor hides
/// anything from it, both ways round.
#[tokio::test]
async fn home_shows_only_the_session_users_progress() {
    let (bridge, abs, alice, alices) = logged_in().await;
    let bob = abs.new_user().await;
    let bobs = bridge.api().login(abs.base_url(), &bob.api_key.key).await;
    listen(&abs, &bob, None, 1200.0, false).await;
    listen(&abs, &bob, Some(0), 60.0, false).await;
    listen(&abs, &bob, Some(2), 0.0, true).await;
    listen(&abs, &alice, Some(1), 30.0, false).await;
    listen(&abs, &alice, None, 3000.0, true).await;
    listen(&abs, &alice, Some(0), 0.0, true).await;

    let home = bridge.api().home(&alices).await;
    assert_eq!(home.continue_listening, [episode(&abs, 1, progress(30.0))]);
    assert_eq!(
        home.latest_episodes,
        [episode(&abs, 2, None), episode(&abs, 1, progress(30.0))]
    );

    let home = bridge.api().home(&bobs).await;
    assert_eq!(
        home.continue_listening,
        [
            episode(&abs, 0, progress(60.0)),
            long_book(&abs, progress(1200.0))
        ]
    );
    assert_eq!(
        home.latest_episodes,
        [episode(&abs, 1, None), episode(&abs, 0, progress(60.0))]
    );
}

/// Audiobookshelf lists a user's Items in progress from every Library, also one an admin
/// hid from them; Home lists only those in Libraries they can see.
#[tokio::test]
async fn home_lists_nothing_from_a_library_the_user_may_not_see() {
    let abs = Audiobookshelf::shared();
    let bridge = TestBridge::start(&named_library_server_toml(abs.base_url(), "Home"));
    let seeded = abs.seeded();
    let books_only = abs.new_user_seeing(&[&seeded.audiobook_library]).await;
    let podcasts_only = abs.new_user_seeing(&[&seeded.podcast_library]).await;
    for user in [&books_only, &podcasts_only] {
        listen(&abs, user, None, 3000.0, false).await;
        listen(&abs, user, Some(1), 300.0, false).await;
    }

    let session = bridge
        .api()
        .login(abs.base_url(), &books_only.api_key.key)
        .await;
    let home = bridge.api().home(&session).await;
    assert_eq!(home.continue_listening, [long_book(&abs, progress(3000.0))]);
    assert_eq!(home.latest_episodes, []);

    let session = bridge
        .api()
        .login(abs.base_url(), &podcasts_only.api_key.key)
        .await;
    let home = bridge.api().home(&session).await;
    assert_eq!(home.continue_listening, [episode(&abs, 1, progress(300.0))]);
    assert_eq!(home.latest_episodes.len(), 3);
}

#[tokio::test]
async fn a_deleted_key_ends_the_session_on_home() {
    let (bridge, abs, alice, session) = logged_in().await;

    abs.delete_api_key(&alice.api_key).await;

    assert_eq!(bridge.api().home_error(&session).await, KEY_REJECTED);
    assert_eq!(bridge.api().home_error(&session).await, SESSION_INVALID);
}

#[tokio::test]
async fn an_outage_on_home_keeps_the_session() {
    let abs = Audiobookshelf::shared();
    let library_server = Forwarder::to(abs.address()).await;
    let bridge = TestBridge::start(&library_server_toml(&library_server.url()));
    let alice = abs.new_user().await;
    let session = bridge
        .api()
        .login(&library_server.url(), &alice.api_key.key)
        .await;

    library_server.stop().await;
    assert_eq!(
        bridge.api().home_error(&session).await,
        (StatusCode::BAD_GATEWAY, ApiError::LibraryServerUnreachable)
    );

    library_server.resume();
    assert_eq!(bridge.api().home(&session).await.latest_episodes.len(), 3);
}
