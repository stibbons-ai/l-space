//! Local playback: a session plays an Item on the phone from the Bridge's stream, bound to its
//! Local session; it reports its position, which the Bridge writes as Progress for the
//! session's user with their key; the stream dies with the Local session. Against the seeded
//! Audiobookshelf.

use std::sync::Arc;

use api::{ApiError, ItemId, LocalStart, Session};
use reqwest::{Method, StatusCode};
use serde_json::json;
use test_support::audiobookshelf::{AbsUser, Audiobookshelf, EPISODE_DURATIONS};
use test_support::bridge::{
    ITEM_NOT_PLAYABLE, KEY_REJECTED, LIBRARY_SERVER_UNREACHABLE, LocalReport, NO_LOCAL_SESSION,
    NOT_FOUND, SESSION_INVALID, TestBridge, library_server_toml, local_report_body, stream_path_id,
};

use crate::forwarder::Forwarder;
use crate::library_proxy::LibraryProxy;

/// A Bridge with the Library Server at `url` (which reaches `abs`), and a user logged in.
struct Setup {
    abs: Arc<Audiobookshelf>,
    bridge: TestBridge,
    user: AbsUser,
    session: Session,
}

async fn setup() -> Setup {
    let abs = Audiobookshelf::shared();
    setup_on(Arc::clone(&abs), abs.base_url()).await
}

async fn setup_on(abs: Arc<Audiobookshelf>, url: &str) -> Setup {
    let bridge = TestBridge::start(&library_server_toml(url));
    let user = abs.new_user().await;
    let session = bridge.api().login(url, &user.api_key.key).await;
    Setup {
        abs,
        bridge,
        user,
        session,
    }
}

impl Setup {
    /// Episode `n` (0-based) of the seeded podcast.
    fn episode(&self, n: usize) -> ItemId {
        self.abs.seeded().episode(n)
    }

    /// The seeded one-file audiobook "Silence", without a cover.
    fn silence(&self) -> ItemId {
        ItemId {
            library_item: self.abs.seeded().silence.clone(),
            episode: None,
        }
    }

    async fn start(&self, item: &ItemId) -> LocalStart {
        self.bridge.api().start_local(&self.session, item).await
    }

    /// Reports (or ends) the Local session `started` answered for, as the user's session.
    async fn report(
        &self,
        started: &LocalStart,
        report: LocalReport,
        position: f64,
        duration: Option<f64>,
    ) {
        self.bridge
            .api()
            .report_local(&self.session, report, &started.stream, position, duration)
            .await;
    }

    /// Reports (or ends) the Local session whose stream's path is `stream` as `session`,
    /// expecting a rejection.
    async fn report_error(
        &self,
        session: &Session,
        stream: &str,
        report: LocalReport,
        position: f64,
    ) -> (StatusCode, ApiError) {
        self.bridge
            .api()
            .report_local_error(session, report, stream, position, None)
            .await
    }

    /// The user's Progress in `item`, straight from Audiobookshelf.
    async fn progress(&self, item: &ItemId) -> Option<(f64, bool)> {
        self.abs.item_progress(&self.user, item).await
    }

    /// `item`'s audio file, straight from Audiobookshelf.
    async fn file(&self, item: &ItemId) -> Vec<u8> {
        let shown = self
            .bridge
            .api()
            .item(&self.session, &item.library_item, item.episode.as_deref())
            .await;
        assert_eq!(shown.audio_files.len(), 1, "{shown:?}");
        self.abs
            .file(&self.user, &item.library_item, &shown.audio_files[0].id)
            .await
    }

    /// The status of a `GET` of `path` on the Bridge, without authenticating.
    async fn status_of(&self, path: &str) -> StatusCode {
        self.bridge
            .api()
            .fetch(Method::GET, path, None)
            .await
            .status()
    }
}

#[tokio::test]
async fn local_playback_starts_at_the_users_progress_and_streams_the_file() {
    let setup = setup().await;
    let episode = setup.episode(1);
    setup
        .abs
        .set_progress(
            &setup.user,
            &episode.library_item,
            episode.episode.as_deref(),
            123.0,
            false,
        )
        .await;

    let started = setup.start(&episode).await;

    assert_eq!(started.item, episode);
    assert_eq!(started.title, "Episode 2");
    assert_eq!(started.podcast.as_deref(), Some("The Clacks Hour"));
    assert_eq!(started.author, None);
    assert_eq!(started.start_at, 123.0);
    assert_eq!(started.duration, Some(EPISODE_DURATIONS[1]));
    // A path on the Bridge, for the App to append to its own Bridge URL: no address in it.
    assert!(started.stream.starts_with("/stream/"), "{}", started.stream);
    assert!(
        started.stream.ends_with("/Episode%202.wav"),
        "{}",
        started.stream
    );
    let id = stream_path_id(&started.stream);
    assert!(
        id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{id}"
    );
    assert!(!started.stream.contains(&setup.user.api_key.key));
    assert_eq!(started.artwork, Some(format!("/cover/{id}")));

    let api = setup.bridge.api();
    let whole = api.fetch(Method::GET, &started.stream, None).await;
    assert_eq!(whole.status(), StatusCode::OK);
    assert_eq!(whole.headers()["content-type"], "audio/wav");
    let file = setup.file(&episode).await;
    assert!(whole.bytes().await.unwrap().to_vec() == file);
    let part = api
        .fetch(Method::GET, &started.stream, Some("bytes=100-199"))
        .await;
    assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        part.headers()["content-range"],
        format!("bytes 100-199/{}", file.len()).as_str()
    );
    assert!(part.bytes().await.unwrap().to_vec() == file[100..200]);

    // The podcast's cover, as an image.
    let cover = api
        .fetch(Method::GET, started.artwork.as_deref().unwrap(), None)
        .await;
    assert_eq!(cover.status(), StatusCode::OK);
    let content_type = cover.headers()["content-type"].to_str().unwrap().to_owned();
    let bytes = cover.bytes().await.unwrap();
    match content_type.as_str() {
        "image/jpeg" => assert!(bytes.starts_with(&[0xFF, 0xD8]), "{content_type}"),
        "image/webp" => assert!(bytes.starts_with(b"RIFF"), "{content_type}"),
        "image/png" => assert!(bytes.starts_with(b"\x89PNG"), "{content_type}"),
        other => panic!("not an image: {other}"),
    }

    // Starting writes nothing.
    assert_eq!(setup.progress(&episode).await, Some((123.0, false)));
}

#[tokio::test]
async fn a_finished_item_or_one_without_progress_starts_at_the_beginning() {
    let setup = setup().await;
    let episode = setup.episode(0);
    setup
        .abs
        .set_progress(
            &setup.user,
            &episode.library_item,
            episode.episode.as_deref(),
            590.0,
            true,
        )
        .await;

    assert_eq!(setup.start(&episode).await.start_at, 0.0);

    // An audiobook without Progress or a cover.
    let silence = setup.start(&setup.silence()).await;
    assert_eq!(silence.start_at, 0.0);
    assert_eq!(silence.title, "Silence");
    assert_eq!(silence.author.as_deref(), Some("L-Space"));
    assert_eq!(silence.artwork, None);
    assert!(
        silence
            .stream
            .ends_with("/Folge%203%20%E2%80%93%20%C3%9Cber%20%26%20Co%20%231%20%2850%25%29.wav"),
        "{}",
        silence.stream
    );
    assert_eq!(setup.status_of(&silence.stream).await, StatusCode::OK);
}

#[tokio::test]
async fn reports_and_the_end_write_progress_and_the_end_kills_the_stream() {
    let setup = setup().await;
    let episode = setup.episode(2);
    let started = setup.start(&episode).await;

    setup
        .report(
            &started,
            LocalReport::Report,
            200.0,
            Some(EPISODE_DURATIONS[2]),
        )
        .await;
    assert_eq!(setup.progress(&episode).await, Some((200.0, false)));
    let record = setup
        .abs
        .progress_record(
            &setup.user,
            &episode.library_item,
            episode.episode.as_deref(),
        )
        .await
        .expect("a Progress record");
    assert_eq!(record["duration"], EPISODE_DURATIONS[2]);
    // The Local session goes on.
    assert_eq!(setup.status_of(&started.stream).await, StatusCode::OK);

    setup.report(&started, LocalReport::End, 250.5, None).await;

    assert_eq!(setup.progress(&episode).await, Some((250.5, false)));
    assert_eq!(
        setup.status_of(&started.stream).await,
        StatusCode::NOT_FOUND
    );
    let cover = started.artwork.as_deref().unwrap();
    assert_eq!(setup.status_of(cover).await, StatusCode::NOT_FOUND);
    // Ended already: nothing more to end or report, and nothing written.
    for report in [LocalReport::End, LocalReport::Report] {
        assert_eq!(
            setup
                .report_error(&setup.session, &started.stream, report, 300.0)
                .await,
            NO_LOCAL_SESSION
        );
    }
    assert_eq!(setup.progress(&episode).await, Some((250.5, false)));
}

/// A report within Audiobookshelf's 10 seconds of the end stays unfinished; one at the
/// Item's length writes it Finished.
#[tokio::test]
async fn only_a_report_at_the_items_length_writes_finished() {
    let setup = setup().await;
    let episode = setup.episode(2);
    let length = EPISODE_DURATIONS[2];
    let started = setup.start(&episode).await;
    setup
        .report(&started, LocalReport::Report, 100.0, Some(length))
        .await;

    setup
        .report(&started, LocalReport::Report, length - 5.0, Some(length))
        .await;
    assert_eq!(setup.progress(&episode).await, Some((length - 5.0, false)));

    setup
        .report(&started, LocalReport::End, length, Some(length))
        .await;
    assert_eq!(setup.progress(&episode).await, Some((length, true)));
}

/// The stream serves its own file only: the Local session's id with any other file name is
/// a bare 404, as an unknown id is.
#[tokio::test]
async fn a_local_sessions_stream_serves_only_its_own_file() {
    let setup = setup().await;
    let started = setup.start(&setup.episode(1)).await;
    let id = stream_path_id(&started.stream);

    for path in [
        format!("/stream/{id}/Episode%201.wav"),
        format!("/stream/{id}/cover.png"),
        format!("/stream/{}/Episode%202.wav", "ab".repeat(32)),
        format!("/cover/{}", "ab".repeat(32)),
    ] {
        assert_eq!(
            setup.status_of(&path).await,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    assert_eq!(setup.status_of(&started.stream).await, StatusCode::OK);
}

#[tokio::test]
async fn a_new_start_or_logging_out_kills_the_old_stream() {
    let setup = setup().await;
    let first = setup.start(&setup.episode(0)).await;
    let second = setup.start(&setup.episode(1)).await;

    assert_eq!(setup.status_of(&first.stream).await, StatusCode::NOT_FOUND);
    let first_cover = first.artwork.as_deref().unwrap();
    assert_eq!(setup.status_of(first_cover).await, StatusCode::NOT_FOUND);
    assert_eq!(setup.status_of(&second.stream).await, StatusCode::OK);
    setup.report(&second, LocalReport::Report, 42.0, None).await;
    assert_eq!(setup.progress(&setup.episode(1)).await, Some((42.0, false)));
    assert_eq!(setup.progress(&setup.episode(0)).await, None);

    setup.bridge.api().logout(&setup.session).await;

    assert_eq!(setup.status_of(&second.stream).await, StatusCode::NOT_FOUND);
    let cover = second.artwork.as_deref().unwrap();
    assert_eq!(setup.status_of(cover).await, StatusCode::NOT_FOUND);
}

/// A report or end that names the Local session a new start replaced (one that arrives late,
/// say) writes and ends nothing: the new one plays on, its Progress as it was.
#[tokio::test]
async fn a_late_report_or_end_for_a_replaced_local_session_touches_nothing() {
    let setup = setup().await;
    let (first_item, second_item) = (setup.episode(0), setup.episode(1));
    let first = setup.start(&first_item).await;
    let second = setup.start(&second_item).await;
    setup.report(&second, LocalReport::Report, 50.0, None).await;

    for report in [LocalReport::Report, LocalReport::End] {
        assert_eq!(
            setup
                .report_error(&setup.session, &first.stream, report, 99.0)
                .await,
            NO_LOCAL_SESSION,
            "{report:?}"
        );
    }
    // Nor does a stream path that isn't one.
    for stream in ["", "/stream", "/cover/abc", "Episode%202.wav"] {
        assert_eq!(
            setup
                .report_error(&setup.session, stream, LocalReport::End, 99.0)
                .await,
            NO_LOCAL_SESSION,
            "{stream:?}"
        );
    }

    assert_eq!(setup.status_of(&second.stream).await, StatusCode::OK);
    let cover = second.artwork.as_deref().unwrap();
    assert_eq!(setup.status_of(cover).await, StatusCode::OK);
    assert_eq!(setup.progress(&second_item).await, Some((50.0, false)));
    assert_eq!(setup.progress(&first_item).await, None);
}

#[tokio::test]
async fn a_revoked_key_kills_the_stream() {
    let setup = setup().await;
    let started = setup.start(&setup.episode(0)).await;

    setup.abs.delete_api_key(&setup.user.api_key).await;
    assert_eq!(
        setup
            .report_error(&setup.session, &started.stream, LocalReport::Report, 10.0)
            .await,
        KEY_REJECTED
    );

    assert_eq!(
        setup.status_of(&started.stream).await,
        StatusCode::NOT_FOUND
    );
    let cover = started.artwork.as_deref().unwrap();
    assert_eq!(setup.status_of(cover).await, StatusCode::NOT_FOUND);
    assert_eq!(
        setup.bridge.api().current_user_error(&setup.session).await,
        SESSION_INVALID
    );
}

/// A Progress write the Library Server rejects revokes the key: the session and its Local
/// session end, for a report and for an end alike.
#[tokio::test]
async fn a_rejected_progress_write_revokes_the_key_and_ends_the_local_session() {
    for report in [LocalReport::Report, LocalReport::End] {
        let abs = Audiobookshelf::shared();
        let proxy = LibraryProxy::to(abs.base_url()).await;
        let setup = setup_on(Arc::clone(&abs), &proxy.url()).await;
        let episode = setup.episode(0);
        let started = setup.start(&episode).await;

        proxy.reject_progress_writes();
        assert_eq!(
            setup
                .report_error(&setup.session, &started.stream, report, 10.0)
                .await,
            KEY_REJECTED,
            "{report:?}"
        );

        assert_eq!(
            setup.status_of(&started.stream).await,
            StatusCode::NOT_FOUND
        );
        let cover = started.artwork.as_deref().unwrap();
        assert_eq!(setup.status_of(cover).await, StatusCode::NOT_FOUND);
        assert_eq!(
            setup.bridge.api().current_user_error(&setup.session).await,
            SESSION_INVALID
        );
        assert_eq!(setup.progress(&episode).await, None);
    }
}

/// A key that now acts for another user (an admin moved it) is revoked on a report or an end,
/// as one the Library Server rejects: the session and its Local session end, and neither user
/// gets Progress.
#[tokio::test]
async fn a_key_moved_to_another_user_is_revoked_and_writes_nothing() {
    for report in [LocalReport::Report, LocalReport::End] {
        let setup = setup().await;
        let episode = setup.episode(0);
        let started = setup.start(&episode).await;
        let other = setup.abs.new_user().await;
        let key = &setup.user.api_key;
        setup.abs.move_api_key(key, &other.id).await;

        assert_eq!(
            setup
                .report_error(&setup.session, &started.stream, report, 10.0)
                .await,
            KEY_REJECTED,
            "{report:?}"
        );

        assert_eq!(
            setup.status_of(&started.stream).await,
            StatusCode::NOT_FOUND
        );
        let cover = started.artwork.as_deref().unwrap();
        assert_eq!(setup.status_of(cover).await, StatusCode::NOT_FOUND);
        assert_eq!(
            setup.bridge.api().current_user_error(&setup.session).await,
            SESSION_INVALID
        );
        assert_eq!(setup.abs.item_progress(&other, &episode).await, None);
        setup.abs.move_api_key(key, &setup.user.id).await;
        assert_eq!(setup.progress(&episode).await, None);
    }
}

/// Another session, of the same user or another, can't report on, end or reach a session's
/// Local session, even naming its stream; the session's own stream and reports go on.
#[tokio::test]
async fn another_session_cannot_reach_a_local_session() {
    let setup = setup().await;
    let episode = setup.episode(1);
    let started = setup.start(&episode).await;
    let api = setup.bridge.api();
    let same_user = api
        .login(setup.abs.base_url(), &setup.user.api_key.key)
        .await;
    let other_user = setup.abs.new_user().await;
    let other = api
        .login(setup.abs.base_url(), &other_user.api_key.key)
        .await;

    for session in [&same_user, &other] {
        for report in [LocalReport::Report, LocalReport::End] {
            assert_eq!(
                setup
                    .report_error(session, &started.stream, report, 77.0)
                    .await,
                NO_LOCAL_SESSION
            );
        }
    }
    assert_eq!(setup.progress(&episode).await, None);
    assert_eq!(setup.status_of(&started.stream).await, StatusCode::OK);

    // Their own Local sessions don't touch it either.
    let theirs = api.start_local(&other, &setup.episode(0)).await;
    api.report_local(&other, LocalReport::End, &theirs.stream, 5.0, None)
        .await;
    assert_eq!(setup.status_of(&theirs.stream).await, StatusCode::NOT_FOUND);
    assert_eq!(setup.status_of(&started.stream).await, StatusCode::OK);
    setup
        .report(&started, LocalReport::Report, 88.0, None)
        .await;
    assert_eq!(setup.progress(&episode).await, Some((88.0, false)));
}

#[tokio::test]
async fn local_playback_needs_a_session_and_a_playable_item() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let item = json!({"item": setup.episode(0)});
    let position = local_report_body("/stream/ab/x.wav", 1.0, None);
    for (path, body) in [
        ("/api/local/start", &item),
        ("/api/local/report", &position),
        ("/api/local/end", &position),
    ] {
        assert_eq!(
            api.post_error(path, None, body.clone()).await,
            SESSION_INVALID,
            "{path}"
        );
    }

    let seeded = setup.abs.seeded();
    let unknown = ItemId {
        library_item: "no-such-item".into(),
        episode: None,
    };
    let no_such_episode = ItemId {
        library_item: seeded.podcast.clone(),
        episode: Some("no-such-episode".into()),
    };
    for missing in [unknown, no_such_episode] {
        assert_eq!(
            api.start_local_error(&setup.session, &missing).await,
            NOT_FOUND,
            "{missing:?}"
        );
    }
    // Three audio files: not yet (#19).
    let long_book = ItemId {
        library_item: seeded.long_book.clone(),
        episode: None,
    };
    assert_eq!(
        api.start_local_error(&setup.session, &long_book).await,
        ITEM_NOT_PLAYABLE
    );

    // None of those started one.
    assert_eq!(
        setup
            .report_error(&setup.session, "/stream/ab/x.wav", LocalReport::Report, 1.0)
            .await,
        NO_LOCAL_SESSION
    );
}

/// An Item in a format Sonos plays but the iPhone doesn't (Ogg) isn't started on the phone;
/// nothing starts, and the Local session already running goes on.
#[tokio::test]
async fn an_item_the_iphone_doesnt_play_doesnt_start() {
    let abs = Audiobookshelf::shared();
    let proxy = LibraryProxy::to(abs.base_url()).await;
    let setup = setup_on(Arc::clone(&abs), &proxy.url()).await;
    let playing = setup.episode(1);
    let started = setup.start(&playing).await;
    proxy.name_files_ogg();

    assert_eq!(
        setup
            .bridge
            .api()
            .start_local_error(&setup.session, &setup.episode(0))
            .await,
        ITEM_NOT_PLAYABLE
    );
    assert_eq!(setup.status_of(&started.stream).await, StatusCode::OK);
    setup
        .report(&started, LocalReport::Report, 42.0, None)
        .await;
    assert_eq!(setup.progress(&playing).await, Some((42.0, false)));
}

/// A Library Server that can't be reached fails a start (nothing starts) and a report (the
/// Local session goes on, and the next report lands).
#[tokio::test]
async fn an_unreachable_library_server_fails_the_call_and_keeps_the_local_session() {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let episode = setup.episode(0);

    forwarder.stop().await;
    assert_eq!(
        setup
            .bridge
            .api()
            .start_local_error(&setup.session, &episode)
            .await,
        LIBRARY_SERVER_UNREACHABLE
    );

    forwarder.resume();
    let started = setup.start(&episode).await;
    forwarder.stop().await;
    assert_eq!(
        setup
            .report_error(&setup.session, &started.stream, LocalReport::Report, 30.0)
            .await,
        LIBRARY_SERVER_UNREACHABLE
    );

    forwarder.resume();
    assert_eq!(setup.status_of(&started.stream).await, StatusCode::OK);
    setup
        .report(&started, LocalReport::Report, 60.0, None)
        .await;
    assert_eq!(setup.progress(&episode).await, Some((60.0, false)));
}

/// An end the Library Server can't take ends the Local session all the same, and answers
/// why its write failed.
#[tokio::test]
async fn an_end_whose_write_fails_still_ends_the_local_session() {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let episode = setup.episode(0);
    let started = setup.start(&episode).await;

    forwarder.stop().await;
    assert_eq!(
        setup
            .report_error(&setup.session, &started.stream, LocalReport::End, 30.0)
            .await,
        LIBRARY_SERVER_UNREACHABLE
    );

    forwarder.resume();
    assert_eq!(
        setup.status_of(&started.stream).await,
        StatusCode::NOT_FOUND
    );
    let cover = started.artwork.as_deref().unwrap();
    assert_eq!(setup.status_of(cover).await, StatusCode::NOT_FOUND);
    assert_eq!(setup.progress(&episode).await, None);
    // The session goes on.
    setup.bridge.api().current_user(&setup.session).await;
}
