//! Browsing a session's Library Server as its user: Libraries, their audiobooks and podcasts,
//! a podcast's episodes by the user's Progress, and Items with chapters and audio files,
//! against the seeded Audiobookshelf.

use std::sync::Arc;

use api::{
    AudioFile, Chapter, EpisodeFilter, Item, ItemId, Library, LibraryEntry, LibraryKind, Progress,
    Session,
};
use serde_json::json;

use test_support::audiobookshelf::{AbsUser, Audiobookshelf, EPISODE_PUBLISHED_AT};
use test_support::bridge::{
    AppApi, KEY_REJECTED, NOT_FOUND, SESSION_INVALID, TestBridge, filter_name, item_query,
    named_library_server_toml,
};

/// A Bridge with the shared Audiobookshelf as its one Library Server, and a new user logged in
/// on it.
async fn logged_in() -> (TestBridge, Arc<Audiobookshelf>, AbsUser, Session) {
    let abs = Audiobookshelf::shared();
    let bridge = TestBridge::start(&named_library_server_toml(abs.base_url(), "Home"));
    let user = abs.new_user().await;
    let session = bridge.api().login(abs.base_url(), &user.api_key.key).await;
    (bridge, abs, user, session)
}

/// The titles of `podcast`'s episodes `filter` picks for `session`, as listed.
async fn episode_titles(
    api: &AppApi,
    session: &Session,
    podcast: &str,
    filter: EpisodeFilter,
) -> Vec<String> {
    api.podcast(session, podcast, filter)
        .await
        .episodes
        .into_iter()
        .map(|episode| episode.title)
        .collect()
}

#[tokio::test]
async fn every_library_of_both_kinds_is_listed() {
    let (bridge, abs, _, session) = logged_in().await;
    let seeded = abs.seeded();

    assert_eq!(
        bridge.api().libraries(&session).await,
        [
            Library {
                id: seeded.audiobook_library.clone(),
                name: "Audiobooks".into(),
                kind: LibraryKind::Audiobooks,
            },
            Library {
                id: seeded.podcast_library.clone(),
                name: "Podcasts".into(),
                kind: LibraryKind::Podcasts,
            },
        ]
    );
}

#[tokio::test]
async fn a_library_lists_its_audiobooks_or_podcasts_by_title() {
    let (bridge, abs, _, session) = logged_in().await;
    let seeded = abs.seeded();

    let entries = bridge
        .api()
        .library_entries(&session, &seeded.audiobook_library)
        .await;
    let titles: Vec<_> = entries.iter().map(|entry| entry.title.as_str()).collect();
    assert_eq!(titles, ["Silence", "The Long Book"]);
    assert_eq!(
        entries[1],
        LibraryEntry {
            id: seeded.long_book.clone(),
            title: "The Long Book".into(),
            author: Some("Pratchett".into()),
        }
    );

    assert_eq!(
        bridge
            .api()
            .library_entries(&session, &seeded.podcast_library)
            .await,
        [LibraryEntry {
            id: seeded.podcast.clone(),
            title: "The Clacks Hour".into(),
            author: Some("Ankh-Morpork Radio".into()),
        }]
    );
}

#[tokio::test]
async fn episodes_are_filtered_by_the_users_progress_newest_first() {
    let (bridge, abs, user, session) = logged_in().await;
    let seeded = abs.seeded();
    let api = bridge.api();

    // Nothing started: every episode is unfinished.
    assert_eq!(
        episode_titles(api, &session, &seeded.podcast, EpisodeFilter::Unfinished).await,
        ["Episode 3", "Episode 2", "Episode 1"]
    );
    assert!(
        episode_titles(api, &session, &seeded.podcast, EpisodeFilter::Finished)
            .await
            .is_empty()
    );

    abs.set_progress(&user, &seeded.podcast, Some(&seeded.episodes[0]), 0.0, true)
        .await;
    abs.set_progress(
        &user,
        &seeded.podcast,
        Some(&seeded.episodes[1]),
        300.0,
        false,
    )
    .await;

    assert_eq!(
        episode_titles(api, &session, &seeded.podcast, EpisodeFilter::Unfinished).await,
        ["Episode 3", "Episode 2"]
    );
    assert_eq!(
        episode_titles(api, &session, &seeded.podcast, EpisodeFilter::Finished).await,
        ["Episode 1"]
    );
    let all = api
        .podcast(&session, &seeded.podcast, EpisodeFilter::All)
        .await;
    assert_eq!(all.title, "The Clacks Hour");
    assert_eq!(all.author.as_deref(), Some("Ankh-Morpork Radio"));
    let listed: Vec<_> = all
        .episodes
        .iter()
        .map(|episode| {
            (
                episode.title.as_str(),
                episode.duration,
                episode.published_at,
                episode.progress.clone(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            ("Episode 3", 3960.0, Some(EPISODE_PUBLISHED_AT[2]), None),
            (
                "Episode 2",
                900.0,
                Some(EPISODE_PUBLISHED_AT[1]),
                Some(Progress {
                    current_time: 300.0,
                    finished: false
                })
            ),
            (
                "Episode 1",
                600.0,
                Some(EPISODE_PUBLISHED_AT[0]),
                Some(Progress {
                    current_time: 0.0,
                    finished: true
                })
            ),
        ]
    );
}

#[tokio::test]
async fn an_audiobook_shows_details_progress_chapters_and_audio_files_at_their_offsets() {
    let (bridge, abs, user, session) = logged_in().await;
    let seeded = abs.seeded();
    abs.set_progress(&user, &seeded.long_book, None, 3000.0, false)
        .await;

    let chapter = |title: &str, start: f64, end: f64| Chapter {
        title: title.into(),
        start,
        end,
    };
    let item = bridge.api().item(&session, &seeded.long_book, None).await;
    let file_ids: Vec<_> = item
        .audio_files
        .iter()
        .map(|file| file.id.clone())
        .collect();
    assert_eq!(
        item,
        Item {
            id: ItemId {
                library_item: seeded.long_book.clone(),
                episode: None,
            },
            title: "The Long Book".into(),
            author: Some("Pratchett".into()),
            podcast: None,
            description: Some("A book that goes on.".into()),
            duration: 4320.0,
            published_at: None,
            progress: Some(Progress {
                current_time: 3000.0,
                finished: false,
            }),
            chapters: vec![
                chapter("01 - Part One", 0.0, 2400.0),
                chapter("02 - Part Two", 2400.0, 4200.0),
                chapter("03 - Part Three", 4200.0, 4320.0),
            ],
            audio_files: [(0.0, 2400.0), (2400.0, 1800.0), (4200.0, 120.0)]
                .into_iter()
                .zip(&file_ids)
                .map(|((start_offset, duration), id)| AudioFile {
                    id: id.clone(),
                    start_offset,
                    duration,
                })
                .collect(),
        }
    );
    // Three distinct files.
    assert!(file_ids[0] != file_ids[1] && file_ids[1] != file_ids[2] && file_ids[0] != file_ids[2]);
}

#[tokio::test]
async fn an_episode_shows_its_podcast_details_and_progress() {
    let (bridge, abs, user, session) = logged_in().await;
    let seeded = abs.seeded();
    let episode = &seeded.episodes[0];
    abs.set_progress(&user, &seeded.podcast, Some(episode), 0.0, true)
        .await;

    let item = bridge
        .api()
        .item(&session, &seeded.podcast, Some(episode))
        .await;

    assert_eq!(item.audio_files.len(), 1);
    assert_eq!(
        Item {
            audio_files: Vec::new(),
            ..item.clone()
        },
        Item {
            id: ItemId {
                library_item: seeded.podcast.clone(),
                episode: Some(episode.clone()),
            },
            title: "Episode 1".into(),
            author: None,
            podcast: Some("The Clacks Hour".into()),
            description: Some("The first one.".into()),
            duration: 600.0,
            published_at: Some(EPISODE_PUBLISHED_AT[0]),
            progress: Some(Progress {
                current_time: 0.0,
                finished: true,
            }),
            chapters: Vec::new(),
            audio_files: Vec::new(),
        }
    );
    assert_eq!(
        (
            item.audio_files[0].start_offset,
            item.audio_files[0].duration
        ),
        (0.0, 600.0)
    );
}

/// Pins the wire shape the App decodes, independent of the shared Rust types.
#[tokio::test]
async fn item_wire_shape() {
    let (bridge, abs, _, session) = logged_in().await;
    let seeded = abs.seeded();

    let mut body: serde_json::Value = bridge
        .api()
        .get_as(&session, "/api/item", &item_query(&seeded.long_book, None))
        .await;
    for file in body["audio_files"].as_array_mut().unwrap() {
        assert!(file["id"].is_string(), "{file}");
        file["id"] = json!(null);
    }
    assert_eq!(
        body,
        json!({
            "id": {"library_item": seeded.long_book, "episode": null},
            "title": "The Long Book",
            "author": "Pratchett",
            "podcast": null,
            "description": "A book that goes on.",
            "duration": 4320.0,
            "published_at": null,
            "progress": null,
            "chapters": [
                {"title": "01 - Part One", "start": 0.0, "end": 2400.0},
                {"title": "02 - Part Two", "start": 2400.0, "end": 4200.0},
                {"title": "03 - Part Three", "start": 4200.0, "end": 4320.0},
            ],
            "audio_files": [
                {"id": null, "start_offset": 0.0, "duration": 2400.0},
                {"id": null, "start_offset": 2400.0, "duration": 1800.0},
                {"id": null, "start_offset": 4200.0, "duration": 120.0},
            ],
        })
    );
}

#[tokio::test]
async fn ids_that_name_nothing_are_not_found() {
    let (bridge, abs, _, session) = logged_in().await;
    let seeded = abs.seeded();
    let api = bridge.api();
    let unknown = "00000000-0000-0000-0000-000000000000";

    for library in [unknown, "../items", "a/b", "", &seeded.long_book] {
        assert_eq!(
            api.get_as_error(&session, "/api/library-entries", &[("library", library)])
                .await,
            NOT_FOUND,
            "{library:?}"
        );
    }
    for (library_item, episode) in [
        (unknown, None),
        ("../me", None),
        ("..%2Fme", None),
        // An audiobook has no episodes, and an episode needs its podcast.
        (seeded.long_book.as_str(), Some(seeded.episodes[0].as_str())),
        (seeded.podcast.as_str(), None),
        (seeded.podcast.as_str(), Some(unknown)),
        (seeded.long_book.as_str(), Some(unknown)),
    ] {
        assert_eq!(
            api.get_as_error(&session, "/api/item", &item_query(library_item, episode))
                .await,
            NOT_FOUND,
            "{library_item:?} {episode:?}"
        );
    }
    for podcast in [unknown, "../me", seeded.long_book.as_str()] {
        assert_eq!(
            api.get_as_error(
                &session,
                "/api/podcast",
                &[("id", podcast), ("filter", filter_name(EpisodeFilter::All))]
            )
            .await,
            NOT_FOUND,
            "{podcast:?}"
        );
    }
    // The session survives all of that.
    assert_eq!(api.libraries(&session).await.len(), 2);
}

#[tokio::test]
async fn browsing_needs_a_session() {
    let (bridge, abs, _, _) = logged_in().await;
    let seeded = abs.seeded();
    let api = bridge.api();
    let item = item_query(&seeded.long_book, None);
    let calls: [(&str, &[(&str, &str)]); 4] = [
        ("/api/libraries", &[]),
        (
            "/api/library-entries",
            &[("library", &seeded.audiobook_library)],
        ),
        ("/api/item", &item),
        (
            "/api/podcast",
            &[
                ("id", &seeded.podcast),
                ("filter", filter_name(EpisodeFilter::All)),
            ],
        ),
    ];
    for (path, query) in calls {
        for authorization in [None, Some("Bearer not-a-session")] {
            assert_eq!(
                api.get_error(path, query, authorization).await,
                SESSION_INVALID,
                "{path} {authorization:?}"
            );
        }
    }
}

/// Two users on the same Items: each sees only their own Progress.
#[tokio::test]
async fn one_users_progress_never_shows_for_another() {
    let (bridge, abs, alice, alices) = logged_in().await;
    let seeded = abs.seeded();
    let api = bridge.api();
    let bob = abs.new_user().await;
    let bobs = api.login(abs.base_url(), &bob.api_key.key).await;

    abs.set_progress(&alice, &seeded.long_book, None, 3000.0, false)
        .await;
    abs.set_progress(
        &alice,
        &seeded.podcast,
        Some(&seeded.episodes[1]),
        0.0,
        true,
    )
    .await;
    abs.set_progress(
        &bob,
        &seeded.podcast,
        Some(&seeded.episodes[2]),
        60.0,
        false,
    )
    .await;

    let progress = async |session, library_item: &str, episode: Option<&str>| {
        api.item(session, library_item, episode).await.progress
    };
    assert_eq!(
        progress(&alices, &seeded.long_book, None).await,
        Some(Progress {
            current_time: 3000.0,
            finished: false
        })
    );
    assert_eq!(progress(&bobs, &seeded.long_book, None).await, None);
    assert_eq!(
        progress(&bobs, &seeded.podcast, Some(&seeded.episodes[1])).await,
        None
    );
    assert_eq!(
        progress(&alices, &seeded.podcast, Some(&seeded.episodes[2])).await,
        None
    );

    assert_eq!(
        episode_titles(api, &alices, &seeded.podcast, EpisodeFilter::Finished).await,
        ["Episode 2"]
    );
    assert!(
        episode_titles(api, &bobs, &seeded.podcast, EpisodeFilter::Finished)
            .await
            .is_empty()
    );
    let bobs_episodes = api
        .podcast(&bobs, &seeded.podcast, EpisodeFilter::All)
        .await
        .episodes;
    let bobs_progress: Vec<_> = bobs_episodes
        .iter()
        .map(|episode| episode.progress.clone())
        .collect();
    assert_eq!(
        bobs_progress,
        [
            Some(Progress {
                current_time: 60.0,
                finished: false
            }),
            None,
            None
        ]
    );
}

/// An admin moves alice's key to bob: her session must not start showing bob's Progress.
#[tokio::test]
async fn a_key_moved_to_another_user_ends_the_session_instead_of_browsing_as_them() {
    let (bridge, abs, alice, session) = logged_in().await;
    let seeded = abs.seeded();
    let bob = abs.new_user().await;
    abs.set_progress(&bob, &seeded.long_book, None, 100.0, false)
        .await;

    abs.move_api_key(&alice.api_key, &bob.id).await;

    assert_eq!(
        bridge
            .api()
            .get_as_error(&session, "/api/item", &item_query(&seeded.long_book, None))
            .await,
        KEY_REJECTED
    );
    assert_eq!(
        bridge.api().current_user_error(&session).await,
        SESSION_INVALID
    );
}

#[tokio::test]
async fn a_deleted_key_ends_the_session_when_browsing() {
    let (bridge, abs, alice, session) = logged_in().await;

    abs.delete_api_key(&alice.api_key).await;

    assert_eq!(
        bridge
            .api()
            .get_as_error(&session, "/api/libraries", &[])
            .await,
        KEY_REJECTED
    );
    assert_eq!(
        bridge.api().current_user_error(&session).await,
        SESSION_INVALID
    );
}
