//! Marking an Item Finished or unfinished for a session's user, against the seeded
//! Audiobookshelf: every write is read back from Audiobookshelf itself, with the user's own
//! key, not through the Bridge.

use api::{EpisodeFilter, ItemId, Progress};

use test_support::audiobookshelf::Audiobookshelf;
use test_support::bridge::{
    KEY_REJECTED, NOT_FOUND, SESSION_INVALID, TestBridge, mark_finished_body,
    named_library_server_toml,
};

use crate::browse::{episode_titles, logged_in};

fn item_id(library_item: &str, episode: Option<&str>) -> ItemId {
    ItemId {
        library_item: library_item.to_owned(),
        episode: episode.map(str::to_owned),
    }
}

fn progress(current_time: f64, finished: bool) -> Option<Progress> {
    Some(Progress {
        current_time,
        finished,
    })
}

/// For an audiobook and for an episode: Finished, then unfinished, each written to
/// Audiobookshelf and answered with the Item as it then is. The user had no Progress in
/// either before.
#[tokio::test]
async fn an_item_without_progress_is_marked_finished_then_unfinished() {
    let (bridge, abs, user, session) = logged_in().await;
    let seeded = abs.seeded();
    let api = bridge.api();

    for (library_item, episode) in [
        (seeded.long_book.as_str(), None),
        (seeded.podcast.as_str(), Some(seeded.episodes[0].as_str())),
    ] {
        let id = item_id(library_item, episode);
        assert_eq!(abs.progress(&user, library_item, episode).await, None);

        let item = api.mark_finished(&session, &id, true).await;
        assert_eq!(item.id, id);
        assert_eq!(item.progress, progress(0.0, true), "{id:?}");
        assert_eq!(
            abs.progress(&user, library_item, episode).await,
            Some((0.0, true)),
            "{id:?}"
        );

        let item = api.mark_finished(&session, &id, false).await;
        assert_eq!(item.progress, progress(0.0, false), "{id:?}");
        assert_eq!(
            abs.progress(&user, library_item, episode).await,
            Some((0.0, false)),
            "{id:?}"
        );
        // The Item as fetched afterwards agrees.
        assert_eq!(
            api.item(&session, library_item, episode).await.progress,
            progress(0.0, false)
        );
    }
}

/// Marking unfinished an Item the user has no Progress in leaves them Progress at 0 seconds,
/// not Finished: what Audiobookshelf records.
#[tokio::test]
async fn an_item_without_progress_marked_unfinished_gets_progress_at_the_start() {
    let (bridge, abs, user, session) = logged_in().await;
    let seeded = abs.seeded();

    let item = bridge
        .api()
        .mark_finished(&session, &item_id(&seeded.silence, None), false)
        .await;

    assert_eq!(item.progress, progress(0.0, false));
    assert_eq!(
        abs.progress(&user, &seeded.silence, None).await,
        Some((0.0, false))
    );
}

/// Marking Finished keeps the position; marking a Finished Item unfinished sets it back to the
/// start, as Audiobookshelf does.
#[tokio::test]
async fn marking_keeps_the_position_until_audiobookshelf_resets_it() {
    let (bridge, abs, user, session) = logged_in().await;
    let seeded = abs.seeded();
    let api = bridge.api();
    let episode = seeded.episodes[2].as_str();
    abs.set_progress(&user, &seeded.long_book, None, 3000.0, false)
        .await;
    abs.set_progress(&user, &seeded.podcast, Some(episode), 600.0, false)
        .await;

    for (library_item, episode, position) in [
        (seeded.long_book.as_str(), None, 3000.0),
        (seeded.podcast.as_str(), Some(episode), 600.0),
    ] {
        let id = item_id(library_item, episode);
        let item = api.mark_finished(&session, &id, true).await;
        assert_eq!(item.progress, progress(position, true), "{id:?}");
        assert_eq!(
            abs.progress(&user, library_item, episode).await,
            Some((position, true))
        );

        let item = api.mark_finished(&session, &id, false).await;
        assert_eq!(item.progress, progress(0.0, false), "{id:?}");
        assert_eq!(
            abs.progress(&user, library_item, episode).await,
            Some((0.0, false))
        );
    }
}

/// The podcast's episode filters pick a marked episode by its new state right away.
#[tokio::test]
async fn the_episode_filters_follow_a_marked_episode() {
    let (bridge, abs, _, session) = logged_in().await;
    let seeded = abs.seeded();
    let api = bridge.api();
    let titles = async |filter| episode_titles(api, &session, &seeded.podcast, filter).await;
    let episode_2 = item_id(&seeded.podcast, Some(&seeded.episodes[1]));

    api.mark_finished(&session, &episode_2, true).await;
    assert_eq!(titles(EpisodeFilter::Finished).await, ["Episode 2"]);
    assert_eq!(
        titles(EpisodeFilter::Unfinished).await,
        ["Episode 3", "Episode 1"]
    );

    api.mark_finished(&session, &episode_2, false).await;
    assert!(titles(EpisodeFilter::Finished).await.is_empty());
    assert_eq!(
        titles(EpisodeFilter::Unfinished).await,
        ["Episode 3", "Episode 2", "Episode 1"]
    );
}

/// Alice marking Items changes nothing of Bob's on the same Items: neither his Progress nor
/// its absence.
#[tokio::test]
async fn marking_changes_only_the_session_users_progress() {
    let (bridge, abs, _, alices) = logged_in().await;
    let seeded = abs.seeded();
    let api = bridge.api();
    let bob = abs.new_user().await;
    abs.set_progress(&bob, &seeded.long_book, None, 1200.0, false)
        .await;
    abs.set_progress(&bob, &seeded.podcast, Some(&seeded.episodes[0]), 0.0, true)
        .await;

    for finished in [true, false] {
        for (library_item, episode) in [
            (seeded.long_book.as_str(), None),
            (seeded.silence.as_str(), None),
            (seeded.podcast.as_str(), Some(seeded.episodes[0].as_str())),
            (seeded.podcast.as_str(), Some(seeded.episodes[1].as_str())),
        ] {
            api.mark_finished(&alices, &item_id(library_item, episode), finished)
                .await;
        }
        assert_eq!(
            abs.progress(&bob, &seeded.long_book, None).await,
            Some((1200.0, false))
        );
        assert_eq!(abs.progress(&bob, &seeded.silence, None).await, None);
        assert_eq!(
            abs.progress(&bob, &seeded.podcast, Some(&seeded.episodes[0]))
                .await,
            Some((0.0, true))
        );
        assert_eq!(
            abs.progress(&bob, &seeded.podcast, Some(&seeded.episodes[1]))
                .await,
            None
        );
    }
}

/// Ids that name no Item the user can see are not found, like fetching the Item, and nothing
/// is written; the session survives.
#[tokio::test]
async fn ids_that_name_nothing_are_not_found_and_nothing_is_written() {
    let (bridge, abs, user, session) = logged_in().await;
    let seeded = abs.seeded();
    let api = bridge.api();
    let unknown = "00000000-0000-0000-0000-000000000000";

    for (library_item, episode) in [
        (unknown, None),
        ("../me", None),
        ("..%2Fme", None),
        (seeded.ebook_only.as_str(), None),
        // Audiobookshelf would write these: it finds an episode by its own id alone.
        (seeded.long_book.as_str(), Some(seeded.episodes[0].as_str())),
        (unknown, Some(seeded.episodes[0].as_str())),
        (seeded.podcast.as_str(), None),
        (seeded.podcast.as_str(), Some(unknown)),
        (seeded.podcast.as_str(), Some("../../me")),
    ] {
        for finished in [true, false] {
            assert_eq!(
                api.mark_finished_error(&session, &item_id(library_item, episode), finished)
                    .await,
                NOT_FOUND,
                "{library_item:?} {episode:?}"
            );
        }
    }
    assert_eq!(abs.progress(&user, &seeded.long_book, None).await, None);
    assert_eq!(abs.progress(&user, &seeded.ebook_only, None).await, None);
    for episode in &seeded.episodes {
        assert_eq!(
            abs.progress(&user, &seeded.podcast, Some(episode)).await,
            None
        );
    }
    assert_eq!(api.libraries(&session).await.len(), 2);
}

/// A user an admin restricted to the audiobook Library can't mark the podcast's episodes,
/// though Audiobookshelf itself would let their key write that Progress.
#[tokio::test]
async fn an_item_in_a_library_the_user_may_not_see_is_not_found_and_nothing_is_written() {
    let abs = Audiobookshelf::shared();
    let bridge = TestBridge::start(&named_library_server_toml(abs.base_url(), "Home"));
    let seeded = abs.seeded();
    let user = abs.new_user_seeing(&[&seeded.audiobook_library]).await;
    let session = bridge.api().login(abs.base_url(), &user.api_key.key).await;
    let episode = item_id(&seeded.podcast, Some(&seeded.episodes[0]));

    assert_eq!(
        bridge
            .api()
            .mark_finished_error(&session, &episode, true)
            .await,
        NOT_FOUND
    );
    assert_eq!(
        abs.progress(&user, &seeded.podcast, Some(&seeded.episodes[0]))
            .await,
        None
    );
    // The audiobook Library still works for them.
    let item = bridge
        .api()
        .mark_finished(&session, &item_id(&seeded.long_book, None), true)
        .await;
    assert_eq!(item.progress, progress(0.0, true));
}

#[tokio::test]
async fn marking_needs_a_session() {
    let (bridge, abs, user, _) = logged_in().await;
    let seeded = abs.seeded();
    let body = mark_finished_body(&item_id(&seeded.long_book, None), true);

    for authorization in [None, Some("Bearer not-a-session")] {
        assert_eq!(
            bridge
                .api()
                .post_error("/api/item/finished", authorization, body.clone())
                .await,
            SESSION_INVALID,
            "{authorization:?}"
        );
    }
    assert_eq!(abs.progress(&user, &seeded.long_book, None).await, None);
}

#[tokio::test]
async fn a_deleted_key_ends_the_session_when_marking() {
    let (bridge, abs, alice, session) = logged_in().await;
    let seeded = abs.seeded();
    let id = item_id(&seeded.long_book, None);

    abs.delete_api_key(&alice.api_key).await;

    assert_eq!(
        bridge.api().mark_finished_error(&session, &id, true).await,
        KEY_REJECTED
    );
    assert_eq!(
        bridge.api().mark_finished_error(&session, &id, true).await,
        SESSION_INVALID
    );
}
