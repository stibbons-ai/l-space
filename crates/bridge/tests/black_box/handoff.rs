//! Handoffs: an Item started on a Group plays from the Bridge's stream, bound to that Handoff,
//! which forwards byte ranges to the Library Server; it keeps playing without the App, is
//! listed to every household member, and survives a restart. Against the seeded
//! Audiobookshelf and `sonos-fake` players on loopback, which fetch the stream as speakers do.

use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{ApiError, CurrentUser, Handoff, ItemId, Session, TransportState};
use reqwest::{Method, StatusCode};
use sonos_fake::{FakeTrack, Fault, Household, PlayerId};

use test_support::audiobookshelf::{AbsUser, Audiobookshelf};
use test_support::bridge::{
    KeyEnv, OTHER_KEY, SESSION_INVALID, TestBridge, acting_as, home_with_sonos_toml,
    output_contains,
};

use crate::forwarder::Forwarder;

/// The SOAP action that gives a Group a URL, as `sonos-fake` records it.
const SET_URI: &str = "AVTransport#SetAVTransportURI";

/// The SOAP action that has a Group play, as `sonos-fake` records it.
const PLAY: &str = "AVTransport#Play";

/// The SOAP action that moves a Group to a position, as `sonos-fake` records it.
const SEEK: &str = "AVTransport#Seek";

/// The WARN the Bridge logs when a Group doesn't go to a start's position.
const NOT_SOUGHT: &str = "a Group did not go to a Handoff's start position";

/// The WARN the Bridge logs when a Handoff's file breaks off from the Library Server.
const BROKE_OFF: &str = "a Handoff's file broke off while streaming from its Library Server";

/// A household of Kitchen (with Bath) and Office, a Bridge searching it with the shared
/// Audiobookshelf as its Library Server, and a user logged in.
struct Setup {
    abs: Arc<Audiobookshelf>,
    household: Household,
    clock: clock::ManualClock,
    kitchen: PlayerId,
    bath: PlayerId,
    office: PlayerId,
    /// Players of their own Groups named by [`setup_with`], in order.
    extra: Vec<PlayerId>,
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
    setup_with(abs, url, &[]).await
}

/// [`setup_on`] with players named `extra`, each a Group of its own, in the household too.
async fn setup_with(abs: Arc<Audiobookshelf>, url: &str, extra: &[&str]) -> Setup {
    let (household, clock) = Household::with_manual_clock();
    let kitchen = household.add_player("Kitchen");
    let bath = household.add_player("Bath");
    household.join(&bath, &kitchen);
    let office = household.add_player("Office");
    let extra = extra
        .iter()
        .map(|name| household.add_player(name))
        .collect();
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
        extra,
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
        self.abs.file(&self.user, &item.library_item, &ino).await
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

    /// Records `user`'s Progress in `item` in Audiobookshelf, as their own player would.
    async fn set_progress(&self, user: &AbsUser, item: &ItemId, current_time: f64, finished: bool) {
        self.abs
            .set_progress(
                user,
                &item.library_item,
                item.episode.as_deref(),
                current_time,
                finished,
            )
            .await;
    }

    /// `user`'s Progress in `item`, straight from Audiobookshelf: `(currentTime, isFinished)`.
    async fn progress(&self, user: &AbsUser, item: &ItemId) -> Option<(f64, bool)> {
        self.abs
            .progress(user, &item.library_item, item.episode.as_deref())
            .await
    }

    /// Where `group`'s Group is in its track, and whether it was sent a `Seek`.
    fn position(&self, group: &PlayerId) -> (Duration, bool) {
        (
            self.household.position(group),
            called(&self.household, group, SEEK),
        )
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

/// The names of `response`'s headers, sorted.
fn header_names(response: &reqwest::Response) -> Vec<String> {
    let mut names: Vec<String> = response
        .headers()
        .keys()
        .map(|name| name.to_string())
        .collect();
    names.sort();
    names
}

/// Checks that the Bridge's `output` holds neither `key` nor any stream path, and that no
/// event at INFO or above holds any of `stream_ids`.
fn assert_quiet(output: &[serde_json::Value], key: &str, stream_ids: &[&str]) {
    assert!(!output_contains(output, key));
    assert!(!output_contains(output, "/stream/"));
    let loud = output
        .iter()
        .filter(|event| !matches!(event["level"].as_str(), Some("DEBUG" | "TRACE")));
    for event in loud {
        for id in stream_ids {
            assert!(!event.to_string().contains(id), "{event}");
        }
    }
}

/// Waits up to ten seconds for `done`, checking every 20 ms; panics with `what` if it never is.
async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Whether `player` has been called with `action`.
fn called(household: &Household, player: &PlayerId, action: &str) -> bool {
    household
        .actions_of(player)
        .iter()
        .any(|called| called == action)
}

/// The WARN events with `message` in the Bridge's `output`.
fn warns<'a>(output: &'a [serde_json::Value], message: &str) -> Vec<&'a serde_json::Value> {
    output
        .iter()
        .filter(|event| event["level"] == "WARN" && event["message"] == message)
        .collect()
}

/// GETs `path` as a speaker would, once more if the first answer is a 502: a test that just
/// closed the forwarder's connections may leave the Bridge a closed one to the Library Server.
async fn fetch_after_reconnect(
    bridge: &TestBridge,
    path: &str,
    range: Option<&str>,
) -> reqwest::Response {
    let response = bridge.api().fetch(Method::GET, path, range).await;
    if response.status() != StatusCode::BAD_GATEWAY {
        return response;
    }
    bridge.api().fetch(Method::GET, path, range).await
}

/// `handoffs` in the order of their Groups' ids, which a listing doesn't promise.
fn by_group(mut handoffs: Vec<Handoff>) -> Vec<Handoff> {
    handoffs.sort_by(|a, b| a.group.cmp(&b.group));
    handoffs
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
    assert_quiet(&output, &setup.user.api_key.key, &[id]);
    assert!(!output_contains(&output, id));
    // A Bridge on loopback (as in these tests) warns that speakers can't reach it.
    assert!(output_contains(&output, "listening on a loopback address"));
}

#[tokio::test]
async fn a_file_name_with_reserved_and_non_ascii_characters_streams() {
    let setup = setup().await;
    let silence = ItemId {
        library_item: setup.abs.seeded().silence.clone(),
        episode: None,
    };

    let (_, url) = setup.start(&silence, &setup.kitchen).await;

    assert_eq!(
        path(&url).split('/').nth(3),
        Some("Folge%203%20%E2%80%93%20%C3%9Cber%20%26%20Co%20%231%20%2850%25%29.wav")
    );
    let fetch = setup.household.fetched(&setup.kitchen).await;
    assert_eq!((fetch.status, &fetch.error), (Some(206), &None));
    assert_eq!(fetch.body, setup.file(&silence).await);
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
    assert_only_stream_headers(&whole);
    assert_eq!(header(&whole, "content-type"), Some("audio/wav"));
    assert_eq!(header(&whole, "content-length"), Some(&*length.to_string()));
    assert!(whole.bytes().await.unwrap()[..] == file[..]);

    let beyond = api
        .fetch(Method::GET, stream, Some(&format!("bytes={length}-")))
        .await;
    assert_eq!(beyond.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_only_stream_headers(&beyond);
    assert_eq!(
        header(&beyond, "content-range"),
        Some(&*format!("bytes */{length}"))
    );

    let head = api.fetch(Method::HEAD, stream, None).await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_only_stream_headers(&head);
    assert_eq!(header(&head, "content-type"), Some("audio/wav"));
    assert_eq!(header(&head, "content-length"), Some(&*length.to_string()));
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
        let (status, names) = (response.status(), header_names(&response));
        (status, names, response.bytes().await.unwrap().to_vec())
    };

    // Each Handoff has its own id, serving its own Item.
    assert_ne!(stream_id(&kitchen), stream_id(&office));
    let (status, _, body) = get(path(&kitchen)).await;
    assert_eq!(
        (status, body),
        (StatusCode::PARTIAL_CONTENT, setup.file(&first).await)
    );
    let (status, _, body) = get(path(&office)).await;
    assert_eq!(
        (status, body),
        (StatusCode::PARTIAL_CONTENT, setup.file(&second).await)
    );

    // A made-up id, a real one with another file name, and a path that isn't UTF-8 name
    // nothing: alike, down to the headers.
    let made_up = format!("/stream/{}/Episode%201.wav", "ab".repeat(32));
    let other_file = format!("/stream/{}/Episode%202.wav", stream_id(&kitchen));
    let short = "/stream/x/Episode%201.wav".to_owned();
    let not_utf8 = format!("/stream/{}/%FF.wav", stream_id(&kitchen));
    let (_, names, _) = get(&made_up).await;
    for path in [made_up, other_file, short, not_utf8] {
        assert_eq!(
            get(&path).await,
            (StatusCode::NOT_FOUND, names.clone(), Vec::new()),
            "{path}"
        );
    }
}

#[tokio::test]
async fn a_handoff_replaces_another_source_without_asking() {
    let setup = setup().await;
    let (playing, paused) = (&setup.kitchen, &setup.office);
    let radio = FakeTrack::new("Morning Radio", Duration::from_secs(3600)).by("Station");
    setup.household.play(playing, radio.clone());
    setup.household.play(paused, radio.clone());
    setup.household.pause(paused);
    let (episode, mut started) = (setup.episode(0), Vec::new());
    let file = setup.file(&episode).await;

    for group in [playing, paused] {
        // One call, nothing to confirm.
        let (handoff, url) = setup.start(&episode, group).await;

        assert_eq!(handoff.group, group.uuid());
        started.push(handoff);
        assert!(
            url.starts_with(&format!("{}/stream/", setup.bridge.url())),
            "{url}"
        );
        assert_eq!(setup.household.transport_state(group), "PLAYING");
        let fetch = setup.household.fetched(group).await;
        assert_eq!(
            (fetch.uri.as_str(), fetch.status),
            (url.as_str(), Some(206))
        );
        assert_eq!(fetch.body, file);
    }

    // The Groups list shows the streams playing, not the other source.
    let groups = setup.bridge.api().groups(&setup.session).await;
    for group in [playing, paused] {
        let shown = groups
            .iter()
            .find(|shown| shown.id == group.uuid())
            .expect("the Group is listed");
        assert_eq!(shown.state, Some(TransportState::Playing), "{shown:?}");
        let now_playing = shown.now_playing.as_ref().expect("a track loaded");
        assert_ne!(now_playing.title, radio.title, "{shown:?}");
        let radio_duration = radio.duration.map(|duration| duration.as_secs_f64());
        assert_ne!(now_playing.duration, radio_duration, "{shown:?}");
    }
    let listed = setup.bridge.api().handoffs(&setup.session).await;
    assert_eq!(by_group(listed), by_group(started));
}

#[tokio::test]
async fn a_new_handoff_on_a_group_ends_its_old_one_whoever_starts_it() {
    let setup = setup().await;
    let (first, second) = (setup.episode(0), setup.episode(1));
    let (_, old) = setup.start(&first, &setup.kitchen).await;
    let (office, _) = setup.start(&second, &setup.office).await;
    let other = setup.abs.new_user().await;
    let api = setup.bridge.api();
    let others = api.login(setup.abs.base_url(), &other.api_key.key).await;

    // Another household member replaces the first user's Handoff on Kitchen.
    let new = api
        .start_handoff(&others, &second, setup.kitchen.uuid())
        .await;
    let url = setup.household.uri(&setup.kitchen).expect("a URL");
    assert_ne!(stream_id(&url), stream_id(&old));
    assert_eq!(
        (&new.item, &new.started_by),
        (&second, &acting_as(&setup.abs, &other))
    );
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");

    // The old stream URL is dead, however it is asked for; the new one serves.
    for (method, range) in [
        (Method::GET, None),
        (Method::GET, Some("bytes=0-")),
        (Method::HEAD, None),
        (Method::HEAD, Some("bytes=0-99")),
    ] {
        let response = api.fetch(method.clone(), path(&old), range).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} {range:?}"
        );
        assert!(response.bytes().await.unwrap().is_empty());
    }
    let response = api.fetch(Method::GET, path(&url), Some("bytes=0-")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert!(response.bytes().await.unwrap()[..] == setup.file(&second).await[..]);

    // Only the new Handoff is listed for Kitchen, to everyone; Office's stays.
    let expected = by_group(vec![office, new]);
    for session in [&setup.session, &others] {
        assert_eq!(by_group(api.handoffs(session).await), expected);
    }
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
    let session = api
        .login(setup.abs.base_url(), &setup.user.api_key.key)
        .await;
    assert_eq!(api.handoffs(&session).await.len(), 1);
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
    assert_quiet(&setup.bridge.stop(), &setup.user.api_key.key, &[]);
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
    // It failed at the coordinator's refusal, not before.
    assert!(called(&setup.household, &setup.kitchen, SET_URI));

    // Office hangs; Kitchen still tells the topology.
    setup.household.set_fault(&setup.office, Fault::Hang);
    let start = Instant::now();
    assert_eq!(
        api.start_handoff_error(&setup.session, &episode, setup.office.uuid())
            .await,
        failed
    );
    let elapsed = start.elapsed();
    // It waited for Office's answer until the Bridge's limit, and not much longer.
    let at_least = sonos::PLAY_TIMEOUT - Duration::from_millis(500);
    let bound = sonos::PLAY_TIMEOUT + Duration::from_secs(3);
    assert!(elapsed >= at_least && elapsed < bound, "{elapsed:?}");
    assert!(called(&setup.household, &setup.office, SET_URI));

    assert_eq!(api.handoffs(&setup.session).await, []);
    // Neither failed start's stream id shows in the log at INFO or above.
    let offered: Vec<String> = [&setup.kitchen, &setup.office]
        .into_iter()
        .flat_map(|player| setup.household.offered(player))
        .collect();
    assert_eq!(offered.len(), 2, "{offered:?}");
    let ids: Vec<&str> = offered.iter().map(|uri| stream_id(uri)).collect();
    assert_quiet(&setup.bridge.stop(), &setup.user.api_key.key, &ids);
}

#[tokio::test]
async fn a_start_the_group_refuses_ends_its_previous_handoff() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let (_, old) = setup.start(&setup.episode(0), &setup.kitchen).await;

    setup
        .household
        .set_fault(&setup.kitchen, Fault::RefusePlayback);
    assert_eq!(
        api.start_handoff_error(&setup.session, &setup.episode(1), setup.kitchen.uuid())
            .await,
        (StatusCode::BAD_GATEWAY, ApiError::GroupFailed)
    );

    // Replaced before the Group refused: Kitchen is left with no Handoff.
    let response = api.fetch(Method::GET, path(&old), Some("bytes=0-")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(api.handoffs(&setup.session).await, []);
}

#[tokio::test]
async fn starts_on_one_group_take_turns() {
    let setup = setup().await;
    let api = setup.bridge.api();
    // Office takes each URL a second late.
    setup
        .household
        .set_fault(&setup.office, Fault::SlowPlayback(Duration::from_secs(1)));
    let (first, second) = (setup.episode(0), setup.episode(1));

    let (a, b) = tokio::join!(
        api.start_handoff(&setup.session, &first, setup.office.uuid()),
        api.start_handoff(&setup.session, &second, setup.office.uuid()),
    );
    assert_eq!(
        (a.group.as_str(), b.group.as_str()),
        (setup.office.uuid(), setup.office.uuid())
    );

    // One start's URL and Play, then the other's: never both URLs before a Play.
    let started: Vec<String> = setup
        .household
        .actions_of(&setup.office)
        .into_iter()
        .filter(|action| action == SET_URI || action == PLAY)
        .collect();
    assert_eq!(started, [SET_URI, PLAY, SET_URI, PLAY]);

    // Office plays the stream of the Handoff listed for it.
    let listed = api.handoffs(&setup.session).await;
    assert_eq!(listed.len(), 1);
    let uri = setup
        .household
        .uri(&setup.office)
        .expect("Office was given a URL");
    let response = api.fetch(Method::GET, path(&uri), Some("bytes=0-")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.bytes().await.unwrap().to_vec(),
        setup.file(&listed[0].item).await
    );
}

#[tokio::test]
async fn a_start_on_a_hanging_group_doesnt_hold_up_another_group() {
    let setup = setup().await;
    setup.household.set_fault(&setup.kitchen, Fault::Hang);
    let api = setup.bridge.api();

    let hanging = async {
        let failed = api
            .start_handoff_error(&setup.session, &setup.episode(0), setup.kitchen.uuid())
            .await;
        (failed, Instant::now())
    };
    let other = async {
        // Once Kitchen has been told its URL and hangs on it, Office starts.
        wait_until("Kitchen was never given a URL", || {
            called(&setup.household, &setup.kitchen, SET_URI)
        })
        .await;
        setup.start(&setup.episode(1), &setup.office).await;
        Instant::now()
    };
    let ((failed, kitchen_done), office_done) = tokio::join!(hanging, other);

    assert!(
        office_done < kitchen_done,
        "Kitchen's start ended before Office's"
    );
    assert_eq!(failed, (StatusCode::BAD_GATEWAY, ApiError::GroupFailed));
}

/// Pins that a start finishes when its client hangs up halfway: the Group plays, and the
/// Handoff listed is the one it plays. Today both Dioxus (which runs server functions
/// detached) and the Bridge's own task for the start provide this.
#[tokio::test]
async fn a_start_whose_client_hangs_up_still_finishes() {
    use tokio::io::AsyncWriteExt;

    let setup = setup().await;
    // Office takes each URL a second late, so the client can hang up meanwhile.
    setup
        .household
        .set_fault(&setup.office, Fault::SlowPlayback(Duration::from_secs(1)));
    let episode = setup.episode(1);
    let body = serde_json::json!({"item": episode, "group": setup.office.uuid()}).to_string();
    let addr = setup.bridge.url().trim_start_matches("http://").to_owned();
    let request = format!(
        "POST /api/handoffs/start HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        setup.session.token.expose(),
        body.len()
    );
    let mut client = tokio::net::TcpStream::connect(&addr).await.unwrap();
    client.write_all(request.as_bytes()).await.unwrap();
    wait_until("Office was never given a URL", || {
        called(&setup.household, &setup.office, SET_URI)
    })
    .await;

    // The client hangs up before the answer.
    drop(client);

    wait_until("Office never played", || {
        setup.household.transport_state(&setup.office) == "PLAYING"
    })
    .await;
    let listed = setup.bridge.api().handoffs(&setup.session).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].item, episode);
    let uri = setup.household.uri(&setup.office).expect("a URL");
    let response = setup
        .bridge
        .api()
        .fetch(Method::GET, path(&uri), Some("bytes=0-"))
        .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.bytes().await.unwrap().to_vec(),
        setup.file(&episode).await
    );
    // Logged as started, though nobody was there to be told.
    let output = setup.bridge.stop();
    assert!(
        output.iter().any(|event| event["level"] == "INFO"
            && event["message"] == "Handoff started"
            && event["group"] == setup.office.uuid()),
        "no INFO that the Handoff started"
    );
}

#[tokio::test]
async fn several_ranges_are_not_forwarded() {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let episode = setup.episode(1);
    let (_, url) = setup.start(&episode, &setup.office).await;
    setup.household.fetched(&setup.office).await;

    forwarder.record_requests().await;
    let response = fetch_after_reconnect(&setup.bridge, path(&url), Some("bytes=0-1,5-6")).await;

    // The whole file, as if no range was asked for, and the Library Server was asked for no
    // range either.
    assert_eq!(response.status(), StatusCode::OK);
    assert_only_stream_headers(&response);
    assert!(response.bytes().await.unwrap().to_vec() == setup.file(&episode).await);
    let sent = forwarder.requests().to_ascii_lowercase();
    assert!(sent.contains("/file/"), "{sent}");
    assert!(!sent.contains("range:"), "{sent}");
}

#[tokio::test]
async fn a_speaker_hanging_up_mid_file_logs_nothing() {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let (_, url) = setup.start(&setup.episode(2), &setup.office).await;
    setup.household.fetched(&setup.office).await;

    // The Library Server sends the start of the file, then nothing more for now.
    forwarder.stall_after(50_000).await;
    let mut response = fetch_after_reconnect(&setup.bridge, path(&url), Some("bytes=0-")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let length: usize = header(&response, "content-length")
        .unwrap()
        .parse()
        .unwrap();
    // The start of the file comes (however slowly), then nothing more for 200 ms: the Library
    // Server really stalls, short of the whole file.
    let first = tokio::time::timeout(Duration::from_secs(10), response.chunk())
        .await
        .expect("the start of the file in time")
        .unwrap()
        .expect("the start of the file");
    let mut received = first.len();
    while let Ok(chunk) = tokio::time::timeout(Duration::from_millis(200), response.chunk()).await {
        let chunk = chunk.unwrap().expect("the file ends only after the stall");
        received += chunk.len();
    }
    assert!(received > 0 && received < length, "{received} of {length}");
    drop(response);
    // Give the Bridge time to notice and drop the stream.
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(warns(&setup.bridge.stop(), BROKE_OFF).len(), 0);
}

#[tokio::test]
async fn another_encryption_key_warns_that_sessions_and_handoffs_end() {
    let abs = Audiobookshelf::shared();
    let mut setup = setup_with(Arc::clone(&abs), abs.base_url(), &["TV"]).await;
    // The user's one key is used by their session and two Handoffs...
    setup.start(&setup.episode(0), &setup.kitchen).await;
    setup.start(&setup.episode(2), &setup.extra[0]).await;
    let api = setup.bridge.api();
    // ...and another user's only by their Handoff.
    let other = setup.abs.new_user().await;
    let others = api.login(setup.abs.base_url(), &other.api_key.key).await;
    api.start_handoff(&others, &setup.episode(1), setup.office.uuid())
        .await;
    api.logout(&others).await;

    setup.bridge.restart_with_key(KeyEnv::value(OTHER_KEY));

    let startup = setup.bridge.startup_output();
    // The `keys` count of the WARN with `message`, which must appear at most once.
    let keys = |message: &str| -> Option<serde_json::Value> {
        let found = warns(startup, message);
        assert!(found.len() <= 1, "{message}: more than once");
        found.first().map(|event| event["keys"].clone())
    };
    let sessions = "stored API keys were encrypted with another encryption key; their sessions \
                    end at their next call, and their users must log in again";
    let handoffs = "stored API keys Handoffs use were encrypted with another encryption key; \
                    those Handoffs end";
    assert_eq!(keys(sessions), Some(serde_json::json!(1)));
    // Three Handoffs, two keys.
    assert_eq!(keys(handoffs), Some(serde_json::json!(2)));
}

#[tokio::test]
async fn a_library_server_failing_mid_file_is_logged_and_keeps_the_handoff() {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let episode = setup.episode(2);
    let length = setup.file(&episode).await.len();
    let (_, url) = setup.start(&episode, &setup.office).await;
    setup.household.fetched(&setup.office).await;

    forwarder.cut_after(50_000).await;
    let response = fetch_after_reconnect(&setup.bridge, path(&url), Some("bytes=0-")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let body = response.bytes().await;
    assert!(
        body.as_ref().map_or(true, |body| body.len() < length),
        "the whole file came through"
    );
    let api = setup.bridge.api();
    assert_eq!(api.handoffs(&setup.session).await.len(), 1);

    let output = setup.bridge.stop();
    let broke_off = warns(&output, BROKE_OFF);
    assert_eq!(broke_off.len(), 1);
    assert_eq!(broke_off[0]["library_server"], forwarder.url().as_str());
    let sent = broke_off[0]["sent"].as_u64().expect("bytes sent");
    assert!(sent > 0 && sent < length as u64, "{sent}");
    assert_quiet(&output, &setup.user.api_key.key, &[stream_id(&url)]);
}

#[tokio::test]
async fn a_stream_path_that_isnt_utf8_is_logged_at_debug() {
    let bridge = TestBridge::start_logging("", "info,bridge::stream=debug");

    let response = bridge
        .api()
        .fetch(
            Method::GET,
            &format!("/stream/{}/%FF.wav", "ab".repeat(32)),
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let output = bridge.stop();
    assert!(
        output.iter().any(|event| event["level"] == "DEBUG"
            && event["message"] == "a stream path that names nothing"),
        "no DEBUG event for the rejected path"
    );
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
    assert_quiet(
        &setup.bridge.stop(),
        &setup.user.api_key.key,
        &[stream_id(&url)],
    );
}

#[tokio::test]
async fn a_start_begins_at_the_users_progress_in_whole_seconds() {
    let setup = setup().await;
    let episode = setup.episode(1);
    let silence = ItemId {
        library_item: setup.abs.seeded().silence.clone(),
        episode: None,
    };
    setup
        .set_progress(&setup.user, &episode, 754.6, false)
        .await;
    // An audiobook's Progress is keyed by the library item alone. The seeded one-file
    // audiobook lasts a second, so its Progress here lies past its end, which the Bridge
    // doesn't check and the fake Group, knowing no length for a stream, accepts.
    setup.set_progress(&setup.user, &silence, 42.9, false).await;

    setup.start(&episode, &setup.kitchen).await;
    setup.start(&silence, &setup.office).await;

    assert_eq!(
        setup.position(&setup.kitchen),
        (Duration::from_secs(754), true)
    );
    assert_eq!(
        setup.position(&setup.office),
        (Duration::from_secs(42), true)
    );
    // Told to play first, then to seek.
    let started: Vec<String> = setup
        .household
        .actions_of(&setup.kitchen)
        .into_iter()
        .filter(|action| [SET_URI, PLAY, SEEK].contains(&action.as_str()))
        .collect();
    assert_eq!(started, [SET_URI, PLAY, SEEK]);
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");
    // A start writes no Progress.
    assert_eq!(
        setup.progress(&setup.user, &episode).await,
        Some((754.6, false))
    );
}

#[tokio::test]
async fn a_finished_item_or_one_without_progress_starts_at_0_without_a_seek() {
    let setup = setup().await;
    let (finished, untouched) = (setup.episode(1), setup.episode(0));
    setup
        .set_progress(&setup.user, &finished, 754.6, true)
        .await;

    setup.start(&finished, &setup.kitchen).await;
    setup.start(&untouched, &setup.office).await;

    assert_eq!(setup.position(&setup.kitchen), (Duration::ZERO, false));
    assert_eq!(setup.position(&setup.office), (Duration::ZERO, false));
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");
    assert_eq!(
        setup.progress(&setup.user, &finished).await,
        Some((754.6, true))
    );
}

#[tokio::test]
async fn play_from_start_starts_at_0_whatever_the_progress() {
    let setup = setup().await;
    let episode = setup.episode(1);
    setup
        .set_progress(&setup.user, &episode, 754.6, false)
        .await;

    let handoff = setup
        .bridge
        .api()
        .start_handoff_from_start(&setup.session, &episode, setup.kitchen.uuid())
        .await;

    assert_eq!(handoff.item, episode);
    assert_eq!(setup.position(&setup.kitchen), (Duration::ZERO, false));
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");
}

#[tokio::test]
async fn an_app_that_sends_no_from_start_gets_the_progress() {
    let setup = setup().await;
    let episode = setup.episode(1);
    setup
        .set_progress(&setup.user, &episode, 754.6, false)
        .await;

    // The body an App sent before "Play from start" existed.
    let body = serde_json::json!({"item": episode, "group": setup.kitchen.uuid()});
    setup
        .bridge
        .api()
        .start_handoff_with(&setup.session, body)
        .await;

    assert_eq!(
        setup.position(&setup.kitchen),
        (Duration::from_secs(754), true)
    );
}

#[tokio::test]
async fn another_users_progress_doesnt_move_a_start() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let episode = setup.episode(1);
    let other = setup.abs.new_user().await;
    let others = api.login(setup.abs.base_url(), &other.api_key.key).await;
    setup.set_progress(&other, &episode, 754.6, false).await;

    // The user has none: theirs starts at 0.
    setup.start(&episode, &setup.kitchen).await;
    assert_eq!(setup.position(&setup.kitchen), (Duration::ZERO, false));

    // With Progress of their own, each starts at theirs.
    setup
        .set_progress(&setup.user, &episode, 300.4, false)
        .await;
    setup.start(&episode, &setup.kitchen).await;
    api.start_handoff(&others, &episode, setup.office.uuid())
        .await;
    assert_eq!(
        setup.household.position(&setup.kitchen),
        Duration::from_secs(300)
    );
    assert_eq!(
        setup.household.position(&setup.office),
        Duration::from_secs(754)
    );
}

#[tokio::test]
async fn a_group_that_refuses_or_does_not_answer_the_seek_keeps_no_handoff() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let episode = setup.episode(1);
    setup
        .set_progress(&setup.user, &episode, 754.6, false)
        .await;
    let failed = (StatusCode::BAD_GATEWAY, ApiError::GroupFailed);
    let (_, old) = setup.start(&setup.episode(0), &setup.kitchen).await;

    setup.household.set_fault(&setup.kitchen, Fault::RefuseSeek);
    assert_eq!(
        api.start_handoff_error(&setup.session, &episode, setup.kitchen.uuid())
            .await,
        failed
    );
    // It played, then failed at the refused seek.
    assert!(called(&setup.household, &setup.kitchen, PLAY));
    assert!(called(&setup.household, &setup.kitchen, SEEK));
    // Neither the replaced Handoff nor the new one is left.
    let refused = setup.household.uri(&setup.kitchen).expect("a URL");
    assert_ne!(stream_id(&refused), stream_id(&old));
    for url in [&old, &refused] {
        let response = api.fetch(Method::GET, path(url), Some("bytes=0-")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    setup.household.set_fault(&setup.office, Fault::HangSeek);
    let start = Instant::now();
    assert_eq!(
        api.start_handoff_error(&setup.session, &episode, setup.office.uuid())
            .await,
        failed
    );
    let elapsed = start.elapsed();
    // It waited for Office's answer to the seek until the Bridge's limit, and not much longer.
    let at_least = sonos::CONTROL_TIMEOUT - Duration::from_millis(500);
    let bound = sonos::CONTROL_TIMEOUT + Duration::from_secs(3);
    assert!(elapsed >= at_least && elapsed < bound, "{elapsed:?}");
    assert!(called(&setup.household, &setup.office, SEEK));

    assert_eq!(api.handoffs(&setup.session).await, []);
    let output = setup.bridge.stop();
    let not_sought = warns(&output, NOT_SOUGHT);
    let groups: Vec<_> = not_sought.iter().map(|event| &event["group"]).collect();
    assert_eq!(groups, [setup.kitchen.uuid(), setup.office.uuid()]);
}
