//! Handoffs: an Item started on a Group plays from the Bridge's stream, bound to that Handoff,
//! which forwards byte ranges to the Library Server; it keeps playing without the App, is
//! listed to every household member, and survives a restart. Against the seeded
//! Audiobookshelf and `sonos-fake` players on loopback, which fetch the stream as speakers do.

use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{ApiError, CurrentUser, Handoff, ItemId, Session};
use reqwest::{Method, StatusCode};
use sonos_fake::{Fault, Household, PlayerId};

use test_support::audiobookshelf::{AbsUser, Audiobookshelf};
use test_support::bridge::{
    SESSION_INVALID, TestBridge, acting_as, home_with_sonos_toml, output_contains,
};

use crate::forwarder::Forwarder;

/// A household of Kitchen (with Bath) and Office, a Bridge searching it with the shared
/// Audiobookshelf as its Library Server, and a user logged in.
struct Setup {
    abs: Arc<Audiobookshelf>,
    household: Household,
    clock: clock::ManualClock,
    kitchen: PlayerId,
    bath: PlayerId,
    office: PlayerId,
    bridge: TestBridge,
    user: AbsUser,
    session: Session,
}

async fn setup() -> Setup {
    let abs = Audiobookshelf::shared();
    setup_on(Arc::clone(&abs), abs.base_url()).await
}

/// [`setup`] with the Library Server at `url`, which reaches `abs`.
async fn setup_on(abs: Arc<Audiobookshelf>, url: &str) -> Setup {
    let (household, clock) = Household::with_manual_clock();
    let kitchen = household.add_player("Kitchen");
    let bath = household.add_player("Bath");
    household.join(&bath, &kitchen);
    let office = household.add_player("Office");
    let bridge = TestBridge::start(&home_with_sonos_toml(url, household.ssdp()));
    let user = abs.new_user().await;
    let session = bridge.api().login(url, &user.api_key.key).await;
    Setup {
        abs,
        household,
        clock,
        kitchen,
        bath,
        office,
        bridge,
        user,
        session,
    }
}

impl Setup {
    /// Episode `n` (0-based) of the seeded podcast.
    fn episode(&self, n: usize) -> ItemId {
        let seeded = self.abs.seeded();
        ItemId {
            library_item: seeded.podcast.clone(),
            episode: Some(seeded.episodes[n].clone()),
        }
    }

    /// The Audiobookshelf id of `item`'s one audio file.
    async fn ino(&self, item: &ItemId) -> String {
        let item = self
            .bridge
            .api()
            .item(&self.session, &item.library_item, item.episode.as_deref())
            .await;
        assert_eq!(item.audio_files.len(), 1, "{item:?}");
        item.audio_files[0].id.clone()
    }

    /// `item`'s audio file, straight from Audiobookshelf.
    async fn file(&self, item: &ItemId) -> Vec<u8> {
        let ino = self.ino(item).await;
        let (status, _, body) = self
            .abs
            .file(&self.user, &item.library_item, &ino, None)
            .await;
        assert_eq!(status, StatusCode::OK);
        body
    }

    /// Starts `item` on `group`'s Group as the user, and returns the Handoff and the URL the
    /// Group was given.
    async fn start(&self, item: &ItemId, group: &PlayerId) -> (Handoff, String) {
        let handoff = self
            .bridge
            .api()
            .start_handoff(&self.session, item, group.uuid())
            .await;
        let url = self
            .household
            .uri(group)
            .expect("the Group was given a URL");
        (handoff, url)
    }

    fn started_by(&self) -> CurrentUser {
        acting_as(&self.abs, &self.user)
    }
}

/// The path of a stream URL, to fetch it from the Bridge wherever it listens now.
fn path(url: &str) -> &str {
    let rest = url.strip_prefix("http://").expect("an http URL");
    &rest[rest.find('/').expect("a path")..]
}

/// A stream URL's id: its second path segment.
fn stream_id(url: &str) -> &str {
    path(url).split('/').nth(2).expect("a stream id")
}

/// The headers a stream answer may have: none of Audiobookshelf's own.
const STREAM_HEADERS: [&str; 5] = [
    "accept-ranges",
    "content-length",
    "content-range",
    "content-type",
    "date",
];

fn assert_only_stream_headers(response: &reqwest::Response) {
    for name in response.headers().keys() {
        assert!(
            STREAM_HEADERS.contains(&name.as_str()),
            "unexpected header {name}"
        );
    }
}

fn header<'a>(response: &'a reqwest::Response, name: &str) -> Option<&'a str> {
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().expect("an ASCII header"))
}

#[tokio::test]
async fn choosing_a_group_plays_the_episode_there_from_the_bridges_stream() {
    let setup = setup().await;
    let episode = setup.episode(0);

    let (handoff, url) = setup.start(&episode, &setup.kitchen).await;

    assert_eq!(handoff.group, setup.kitchen.uuid());
    assert_eq!(handoff.item, episode);
    assert_eq!(handoff.started_by, setup.started_by());
    // The Group fetches from the Bridge's stream endpoint, never from the Library Server.
    assert!(
        url.starts_with(&format!("{}/stream/", setup.bridge.url())),
        "{url}"
    );
    assert_eq!(path(&url).split('/').nth(3), Some("Episode%201.wav"));
    assert!(!url.contains(setup.abs.address()), "{url}");
    assert!(!url.contains(&setup.user.api_key.key), "{url}");
    // 256 random bits, hex-encoded.
    let id = stream_id(&url);
    assert!(
        id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{id}"
    );
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");

    // It fetched the episode as a speaker does, a byte range from the start.
    let fetch = setup.household.fetched(&setup.kitchen).await;
    assert_eq!((fetch.status, &fetch.error), (Some(206), &None));
    assert_eq!(fetch.header("content-type"), Some("audio/wav"));
    assert_eq!(fetch.header("accept-ranges"), Some("bytes"));
    assert_eq!(fetch.body, setup.file(&episode).await);

    // The other Groups are untouched.
    for other in [&setup.bath, &setup.office] {
        let started = setup.household.actions_of(other);
        assert!(
            !started
                .iter()
                .any(|action| action.contains("AVTransportURI")),
            "{started:?}"
        );
    }
    assert_eq!(setup.household.uri(&setup.office), None);

    // Neither the key nor the stream id shows in the Bridge's log.
    let output = setup.bridge.stop();
    assert!(!output_contains(&output, &setup.user.api_key.key));
    assert!(!output_contains(&output, id));
}

#[tokio::test]
async fn byte_ranges_come_back_exactly_with_the_media_type() {
    let setup = setup().await;
    let episode = setup.episode(1);
    let (_, url) = setup.start(&episode, &setup.office).await;
    let (stream, file) = (path(&url), setup.file(&episode).await);
    let api = setup.bridge.api();
    let length = file.len();

    let open_ended = format!("bytes={}-", length - 5000);
    let ranges = [
        ("bytes=0-99", 0, 100),
        ("bytes=1000-1999", 1000, 2000),
        (open_ended.as_str(), length - 5000, length),
        ("bytes=-300", length - 300, length),
    ];
    for (range, start, end) in ranges {
        let response = api.fetch(Method::GET, stream, Some(range)).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT, "{range}");
        assert_only_stream_headers(&response);
        assert_eq!(header(&response, "content-type"), Some("audio/wav"));
        assert_eq!(header(&response, "accept-ranges"), Some("bytes"));
        let expected_range = format!("bytes {start}-{}/{length}", end - 1);
        assert_eq!(header(&response, "content-range"), Some(&*expected_range));
        let expected_length = (end - start).to_string();
        assert_eq!(header(&response, "content-length"), Some(&*expected_length));
        let body = response.bytes().await.unwrap();
        assert!(body[..] == file[start..end], "{range}: other bytes");
    }

    let whole = api.fetch(Method::GET, stream, None).await;
    assert_eq!(whole.status(), StatusCode::OK);
    assert_eq!(header(&whole, "content-type"), Some("audio/wav"));
    assert_eq!(header(&whole, "content-length"), Some(&*length.to_string()));
    assert!(whole.bytes().await.unwrap()[..] == file[..]);

    let beyond = api
        .fetch(Method::GET, stream, Some(&format!("bytes={length}-")))
        .await;
    assert_eq!(beyond.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        header(&beyond, "content-range"),
        Some(&*format!("bytes */{length}"))
    );

    let head = api.fetch(Method::HEAD, stream, None).await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_only_stream_headers(&head);
    assert_eq!(header(&head, "content-type"), Some("audio/wav"));
    assert_eq!(header(&head, "content-length"), Some(&*length.to_string()));
    assert!(head.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_stream_serves_only_its_handoffs_file() {
    let setup = setup().await;
    let (first, second) = (setup.episode(0), setup.episode(1));
    let (_, kitchen) = setup.start(&first, &setup.kitchen).await;
    let (_, office) = setup.start(&second, &setup.office).await;
    let api = setup.bridge.api();
    let get = async |path: &str| {
        let response = api.fetch(Method::GET, path, Some("bytes=0-")).await;
        let status = response.status();
        (status, response.bytes().await.unwrap().to_vec())
    };

    // Each Handoff has its own id, serving its own Item.
    assert_ne!(stream_id(&kitchen), stream_id(&office));
    assert_eq!(
        get(path(&kitchen)).await,
        (StatusCode::PARTIAL_CONTENT, setup.file(&first).await)
    );
    assert_eq!(
        get(path(&office)).await,
        (StatusCode::PARTIAL_CONTENT, setup.file(&second).await)
    );

    // A made-up id, and a real one with another file name, name nothing: alike.
    let made_up = format!("/stream/{}/Episode%201.wav", "ab".repeat(32));
    let other_file = format!("/stream/{}/Episode%202.wav", stream_id(&kitchen));
    let short = "/stream/x/Episode%201.wav".to_owned();
    for path in [made_up, other_file, short] {
        assert_eq!(
            get(&path).await,
            (StatusCode::NOT_FOUND, Vec::new()),
            "{path}"
        );
    }

    // A new Handoff on Kitchen replaces its old one, whose URL is dead.
    let (replacing, replaced_by) = setup.start(&second, &setup.kitchen).await;
    assert_ne!(stream_id(&replaced_by), stream_id(&kitchen));
    assert_eq!(get(path(&kitchen)).await.0, StatusCode::NOT_FOUND);
    assert_eq!(get(path(&replaced_by)).await.0, StatusCode::PARTIAL_CONTENT);
    let groups: Vec<String> = api
        .handoffs(&setup.session)
        .await
        .into_iter()
        .map(|handoff| handoff.group)
        .collect();
    assert_eq!(groups, [setup.office.uuid(), replacing.group.as_str()]);
}

#[tokio::test]
async fn playback_goes_on_without_the_app() {
    let setup = setup().await;
    let (_, url) = setup.start(&setup.episode(2), &setup.kitchen).await;
    setup.household.fetched(&setup.kitchen).await;

    // No App API call from here on.
    setup.clock.advance(Duration::from_secs(300));
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");
    assert_eq!(
        setup.household.position(&setup.kitchen),
        Duration::from_secs(300)
    );
    let api = setup.bridge.api();
    let response = api.fetch(Method::GET, path(&url), Some("bytes=0-99")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);

    // Logging out leaves the Handoff, and its stream, to go on.
    api.logout(&setup.session).await;
    setup.clock.advance(Duration::from_secs(300));
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");
    assert_eq!(
        setup.household.position(&setup.kitchen),
        Duration::from_secs(600)
    );
    let response = api
        .fetch(Method::GET, path(&url), Some("bytes=100-199"))
        .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.bytes().await.unwrap().len(), 100);
}

#[tokio::test]
async fn handoffs_are_listed_to_everyone_and_survive_a_restart() {
    let mut setup = setup().await;
    let (handoff, url) = setup.start(&setup.episode(0), &setup.kitchen).await;
    let other = setup.abs.new_user().await;
    let others = setup
        .bridge
        .api()
        .login(setup.abs.base_url(), &other.api_key.key)
        .await;

    for session in [&setup.session, &others] {
        assert_eq!(
            setup.bridge.api().handoffs(session).await,
            std::slice::from_ref(&handoff)
        );
    }

    setup.bridge.restart();
    assert_eq!(setup.bridge.api().handoffs(&others).await, [handoff]);
    // The Bridge may listen on another port now; the stream's path still serves there.
    let response = setup
        .bridge
        .api()
        .fetch(Method::GET, path(&url), Some("bytes=0-99"))
        .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
}

#[tokio::test]
async fn a_start_that_fails_leaves_no_handoff() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let episode = setup.episode(0);
    let seeded = setup.abs.seeded();
    let long_book = ItemId {
        library_item: seeded.long_book.clone(),
        episode: None,
    };
    let no_such_episode = ItemId {
        episode: Some("no-such-episode".into()),
        ..episode.clone()
    };

    let failures = [
        (
            &episode,
            "RINCON_NOSUCH01400",
            StatusCode::NOT_FOUND,
            ApiError::GroupNotFound,
        ),
        // A member that doesn't coordinate its Group is no Group.
        (
            &episode,
            setup.bath.uuid(),
            StatusCode::NOT_FOUND,
            ApiError::GroupNotFound,
        ),
        (
            &episode,
            "../../etc",
            StatusCode::NOT_FOUND,
            ApiError::GroupNotFound,
        ),
        (
            &long_book,
            setup.kitchen.uuid(),
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::ItemNotPlayable,
        ),
        (
            &no_such_episode,
            setup.kitchen.uuid(),
            StatusCode::NOT_FOUND,
            ApiError::NotFound,
        ),
    ];
    for (item, group, status, error) in failures {
        assert_eq!(
            api.start_handoff_error(&setup.session, item, group).await,
            (status, error),
            "{item:?} on {group}"
        );
    }
    assert_eq!(api.handoffs(&setup.session).await, []);
    let started = setup.household.actions();
    assert!(
        !started
            .iter()
            .any(|action| action.contains("AVTransportURI")),
        "{started:?}"
    );

    api.logout(&setup.session).await;
    assert_eq!(
        api.start_handoff_error(&setup.session, &episode, setup.kitchen.uuid())
            .await,
        SESSION_INVALID
    );
}

#[tokio::test]
async fn a_group_that_refuses_or_does_not_answer_keeps_no_handoff() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let episode = setup.episode(0);
    let failed = (StatusCode::BAD_GATEWAY, ApiError::GroupFailed);

    setup
        .household
        .set_fault(&setup.kitchen, Fault::RefusePlayback);
    assert_eq!(
        api.start_handoff_error(&setup.session, &episode, setup.kitchen.uuid())
            .await,
        failed
    );

    // Office hangs; Kitchen still tells the topology.
    setup.household.set_fault(&setup.office, Fault::Hang);
    let start = Instant::now();
    assert_eq!(
        api.start_handoff_error(&setup.session, &episode, setup.office.uuid())
            .await,
        failed
    );
    let bound = sonos::PLAY_TIMEOUT + Duration::from_secs(1);
    assert!(start.elapsed() < bound, "{:?}", start.elapsed());

    assert_eq!(api.handoffs(&setup.session).await, []);
}

#[tokio::test]
async fn a_library_server_that_goes_down_while_streaming_is_a_bad_gateway() {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let (_, url) = setup.start(&setup.episode(0), &setup.office).await;
    let api = setup.bridge.api();

    forwarder.stop().await;
    let start = Instant::now();
    let response = api.fetch(Method::GET, path(&url), Some("bytes=0-99")).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );

    forwarder.resume();
    let response = api.fetch(Method::GET, path(&url), Some("bytes=0-99")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    // The Handoff never ended.
    assert_eq!(api.handoffs(&setup.session).await.len(), 1);
}
