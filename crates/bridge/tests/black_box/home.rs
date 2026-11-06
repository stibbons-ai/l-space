//! Home for a session's user, against the seeded Audiobookshelf: Progress is set through
//! Audiobookshelf's own API with each user's key, as their player would, never through the
//! Bridge.

use std::time::Duration;

use api::{ApiError, HomeEntry, ItemId, Progress};
use reqwest::StatusCode;

use test_support::audiobookshelf::{AbsUser, Audiobookshelf, EPISODE_PUBLISHED_AT};
use test_support::bridge::{
    KEY_REJECTED, SESSION_INVALID, TestBridge, library_server_toml, named_library_server_toml,
};

use crate::browse::logged_in;
use crate::forwarder::Forwarder;

/// The seeded episodes' durations, oldest first.
const EPISODE_DURATIONS: [f64; 3] = [600.0, 900.0, 3960.0];

fn progress(current_time: f64) -> Option<Progress> {
    Some(Progress {
        current_time,
        finished: false,
    })
}

/// "The Long Book" as Home lists it, with `progress`.
fn long_book(abs: &Audiobookshelf, progress: Option<Progress>) -> HomeEntry {
    HomeEntry {
        id: ItemId {
            library_item: abs.seeded().long_book.clone(),
            episode: None,
        },
        title: "The Long Book".into(),
        author: Some("Pratchett".into()),
        podcast: None,
        duration: Some(4320.0),
        published_at: None,
        progress,
    }
}

/// Seeded episode `n` (0 is the oldest) as Home lists it, with `progress`.
fn episode(abs: &Audiobookshelf, n: usize, progress: Option<Progress>) -> HomeEntry {
    let seeded = abs.seeded();
    HomeEntry {
        id: ItemId {
            library_item: seeded.podcast.clone(),
            episode: Some(seeded.episodes[n].clone()),
        },
        title: format!("Episode {}", n + 1),
        author: None,
        podcast: Some("The Clacks Hour".into()),
        duration: Some(EPISODE_DURATIONS[n]),
        published_at: Some(EPISODE_PUBLISHED_AT[n]),
        progress,
    }
}

/// Records `user`'s Progress in episode `n` (or, without one, in "The Long Book") a moment
/// after the last, so Audiobookshelf's update times (milliseconds) tell them apart.
async fn listen(abs: &Audiobookshelf, user: &AbsUser, n: Option<usize>, at: f64, finished: bool) {
    tokio::time::sleep(Duration::from_millis(20)).await;
    let seeded = abs.seeded();
    match n {
        Some(n) => {
            abs.set_progress(
                user,
                &seeded.podcast,
                Some(&seeded.episodes[n]),
                at,
                finished,
            )
            .await;
        }
        None => {
            abs.set_progress(user, &seeded.long_book, None, at, finished)
                .await;
        }
    }
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
/// anything from it, and theirs doesn't reach the other's.
#[tokio::test]
async fn home_shows_only_the_session_users_progress() {
    let (bridge, abs, alice, alices) = logged_in().await;
    let bob = abs.new_user().await;
    let bobs = bridge.api().login(abs.base_url(), &bob.api_key.key).await;
    listen(&abs, &bob, None, 1200.0, false).await;
    listen(&abs, &bob, Some(0), 60.0, false).await;
    listen(&abs, &bob, Some(2), 0.0, true).await;
    listen(&abs, &alice, Some(1), 30.0, false).await;

    let home = bridge.api().home(&alices).await;
    assert_eq!(home.continue_listening, [episode(&abs, 1, progress(30.0))]);
    assert_eq!(
        home.latest_episodes,
        [
            episode(&abs, 2, None),
            episode(&abs, 1, progress(30.0)),
            episode(&abs, 0, None)
        ]
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
async fn home_needs_a_session() {
    let (bridge, _, _, _) = logged_in().await;

    for authorization in [None, Some("Bearer not-a-session")] {
        assert_eq!(
            bridge
                .api()
                .get_error("/api/home", &[], authorization)
                .await,
            SESSION_INVALID,
            "{authorization:?}"
        );
    }
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
