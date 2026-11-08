//! Controlling a Group: pause, resume, 15 s back, 30 s forward and volume, from any session,
//! on whoever's Handoff; pausing and skipping write the Group's position as Progress for the
//! user who started the Handoff, with their stored key. Against the seeded Audiobookshelf
//! (read back with each user's own key) and `sonos-fake` players on the manual clock, so
//! positions are exact.

use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{Handoff, ItemId, Playback, Session, Skip, TransportState};
use sonos_fake::{FakeTrack, Fault, Household, PlayerId};

use test_support::audiobookshelf::{AbsUser, Audiobookshelf};
use test_support::bridge::{
    GROUP_FAILED, GROUP_NOT_FOUND, GroupControl, NO_HANDOFF, SESSION_INVALID, TestBridge,
    home_with_sonos_toml, output_contains,
};

use crate::forwarder::Forwarder;

/// The SOAP actions that control a Group's playback, as `sonos-fake` records them.
const PLAYBACK_CONTROLS: [&str; 3] = ["AVTransport#Pause", "AVTransport#Play", "AVTransport#Seek"];

/// The controls of a Group's Handoff.
const HANDOFF_CONTROLS: [GroupControl; 4] = [
    GroupControl::Pause,
    GroupControl::Resume,
    GroupControl::Skip(Skip::Back15),
    GroupControl::Skip(Skip::Forward30),
];

/// A household of Kitchen (with Bath) and Office, a Bridge searching it with the shared
/// Audiobookshelf (or a forwarder to it) as its Library Server, and two users logged in:
/// Alice, who starts Handoffs, and Bob, who controls them.
struct Setup {
    abs: Arc<Audiobookshelf>,
    household: Household,
    clock: clock::ManualClock,
    kitchen: PlayerId,
    bath: PlayerId,
    office: PlayerId,
    bridge: TestBridge,
    alice: AbsUser,
    alices: Session,
    bob: AbsUser,
    bobs: Session,
}

async fn setup() -> Setup {
    let abs = Audiobookshelf::shared();
    let url = abs.base_url().to_owned();
    setup_on(abs, &url).await
}

/// [`setup`] with the Library Server at `url`, which reaches `abs`.
async fn setup_on(abs: Arc<Audiobookshelf>, url: &str) -> Setup {
    let (household, clock) = Household::with_manual_clock();
    let kitchen = household.add_player("Kitchen");
    let bath = household.add_player("Bath");
    household.join(&bath, &kitchen);
    let office = household.add_player("Office");
    let bridge = TestBridge::start(&home_with_sonos_toml(url, household.ssdp()));
    let (alice, bob) = (abs.new_user().await, abs.new_user().await);
    let alices = bridge.api().login(url, &alice.api_key.key).await;
    let bobs = bridge.api().login(url, &bob.api_key.key).await;
    Setup {
        abs,
        household,
        clock,
        kitchen,
        bath,
        office,
        bridge,
        alice,
        alices,
        bob,
        bobs,
    }
}

impl Setup {
    /// Episode `n` (0-based) of the seeded podcast: 600, 900 and 3960 seconds long.
    fn episode(&self, n: usize) -> ItemId {
        let seeded = self.abs.seeded();
        ItemId {
            library_item: seeded.podcast.clone(),
            episode: Some(seeded.episodes[n].clone()),
        }
    }

    /// Alice starts `item` on `group`'s Group.
    async fn start(&self, item: &ItemId, group: &PlayerId) -> Handoff {
        self.bridge
            .api()
            .start_handoff(&self.alices, item, group.uuid())
            .await
    }

    /// `user`'s Progress in `item`, read from Audiobookshelf with their own key: its
    /// `currentTime`, if any.
    async fn progress(&self, user: &AbsUser, item: &ItemId) -> Option<f64> {
        let progress = self
            .abs
            .progress(user, &item.library_item, item.episode.as_deref())
            .await;
        progress.map(|(current_time, _)| current_time)
    }

    fn position(&self, group: &PlayerId) -> Duration {
        self.household.position(group)
    }

    /// The playback controls `player` was sent.
    fn playback_controls(&self, player: &PlayerId) -> Vec<String> {
        let actions = self.household.actions_of(player).into_iter();
        actions
            .filter(|action| PLAYBACK_CONTROLS.contains(&action.as_str()))
            .collect()
    }
}

fn seconds(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

fn playback(state: TransportState, position: f64, duration: f64) -> Playback {
    Playback {
        state,
        position: Some(position),
        duration: Some(duration),
    }
}

#[tokio::test]
async fn anyone_pauses_and_resumes_a_handoff_and_the_pause_is_the_starters_progress() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let episode = setup.episode(0);
    let handoff = setup.start(&episode, &setup.kitchen).await;
    setup.clock.advance(seconds(120));

    // Bob pauses Alice's Handoff.
    assert_eq!(
        api.pause_group(&setup.bobs, setup.kitchen.uuid()).await,
        playback(TransportState::Paused, 120.0, 600.0)
    );
    assert_eq!(
        setup.household.transport_state(&setup.kitchen),
        "PAUSED_PLAYBACK"
    );
    setup.clock.advance(seconds(60));
    assert_eq!(setup.position(&setup.kitchen), seconds(120));
    // Progress for Alice, who started it; none for Bob.
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(120.0));
    assert_eq!(setup.progress(&setup.bob, &episode).await, None);

    // The Groups list shows it paused, with its Handoff.
    let groups = api.groups(&setup.bobs).await;
    let kitchen = groups.iter().find(|group| group.name == "Kitchen").unwrap();
    assert_eq!(kitchen.state, Some(TransportState::Paused));
    assert_eq!(kitchen.handoff.as_ref(), Some(&handoff));
    let office = groups.iter().find(|group| group.name == "Office").unwrap();
    assert_eq!(office.handoff, None);

    // Bob resumes; it moves on from where it paused, and resuming writes nothing (nor asks
    // the Library Server for the length the Group doesn't know).
    assert_eq!(
        api.resume_group(&setup.bobs, setup.kitchen.uuid()).await,
        Playback {
            state: TransportState::Playing,
            position: Some(120.0),
            duration: None,
        }
    );
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");
    setup.clock.advance(seconds(30));
    assert_eq!(setup.position(&setup.kitchen), seconds(150));
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(120.0));

    // Alice logs out; the Handoff's key still writes her Progress when Bob pauses.
    api.logout(&setup.alices).await;
    api.pause_group(&setup.bobs, setup.kitchen.uuid()).await;
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(150.0));
    assert_eq!(setup.progress(&setup.bob, &episode).await, None);

    // Only the coordinator was controlled (after the start's own Play); the member and Office
    // were not.
    assert_eq!(
        setup.playback_controls(&setup.kitchen),
        [
            "AVTransport#Play",
            "AVTransport#Pause",
            "AVTransport#Play",
            "AVTransport#Pause"
        ]
    );
    assert_eq!(setup.playback_controls(&setup.bath), Vec::<String>::new());
    assert_eq!(setup.playback_controls(&setup.office), Vec::<String>::new());

    // No key in the log.
    let output = setup.bridge.stop();
    assert!(!output_contains(&output, &setup.alice.api_key.key));
    assert!(!output_contains(&output, &setup.bob.api_key.key));
    assert!(!output_contains(&output, "/stream/"));
}

#[tokio::test]
async fn skips_move_15_seconds_back_and_30_forward_within_the_item() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let kitchen = setup.kitchen.uuid();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.clock.advance(seconds(100));

    // Mid-Item, by exactly 15 and 30 seconds, playing on; the starter's Progress follows.
    assert_eq!(
        api.skip_group(&setup.bobs, kitchen, Skip::Back15).await,
        playback(TransportState::Playing, 85.0, 600.0)
    );
    assert_eq!(setup.position(&setup.kitchen), seconds(85));
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(85.0));
    assert_eq!(
        api.skip_group(&setup.bobs, kitchen, Skip::Forward30).await,
        playback(TransportState::Playing, 115.0, 600.0)
    );
    assert_eq!(setup.position(&setup.kitchen), seconds(115));
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(115.0));
    assert_eq!(setup.progress(&setup.bob, &episode).await, None);

    // Paused, it stays paused.
    api.pause_group(&setup.bobs, kitchen).await;
    assert_eq!(
        api.skip_group(&setup.bobs, kitchen, Skip::Forward30).await,
        playback(TransportState::Paused, 145.0, 600.0)
    );
    assert_eq!(
        setup.household.transport_state(&setup.kitchen),
        "PAUSED_PLAYBACK"
    );
    api.resume_group(&setup.bobs, kitchen).await;

    // Near the end, forward stops at the end: the Item is 600 s long, which the Library
    // Server tells (the Group knows no length for a stream).
    setup.clock.advance(seconds(590 - 145));
    assert_eq!(setup.position(&setup.kitchen), seconds(590));
    assert_eq!(
        api.skip_group(&setup.bobs, kitchen, Skip::Forward30).await,
        playback(TransportState::Playing, 600.0, 600.0)
    );
    assert_eq!(setup.position(&setup.kitchen), seconds(600));
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(600.0));

    // Near the start, back stops at the start.
    let other = setup.episode(1);
    setup.start(&other, &setup.office).await;
    setup.clock.advance(seconds(10));
    assert_eq!(
        api.skip_group(&setup.bobs, setup.office.uuid(), Skip::Back15)
            .await,
        playback(TransportState::Playing, 0.0, 900.0)
    );
    assert_eq!(setup.position(&setup.office), Duration::ZERO);
    assert_eq!(setup.progress(&setup.alice, &other).await, Some(0.0));
}

#[tokio::test]
async fn a_groups_own_length_bounds_a_skip() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let episode = setup.episode(2);
    setup.start(&episode, &setup.office).await;
    let stream = setup.household.uri(&setup.office).unwrap();
    // The Group reports a length of its own for the stream, shorter than the Item's 3960 s.
    setup.household.play(
        &setup.office,
        FakeTrack {
            uri: stream,
            ..FakeTrack::new("Episode 3", seconds(700))
        },
    );
    setup.clock.advance(seconds(690));

    assert_eq!(
        api.skip_group(&setup.bobs, setup.office.uuid(), Skip::Forward30)
            .await,
        playback(TransportState::Playing, 700.0, 700.0)
    );
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(700.0));
}

#[tokio::test]
async fn volume_changes_on_any_group_with_or_without_a_handoff() {
    let setup = setup().await;
    let api = setup.bridge.api();
    setup.start(&setup.episode(0), &setup.kitchen).await;

    // The Groups list reports each Group's volume.
    setup.household.set_volume(&setup.office, 25);
    let volumes = |groups: Vec<api::Group>| -> Vec<(String, Option<u8>)> {
        let groups = groups.into_iter();
        groups.map(|group| (group.name, group.volume)).collect()
    };
    assert_eq!(
        volumes(api.groups(&setup.bobs).await),
        [
            ("Kitchen".to_owned(), Some(sonos_fake::DEFAULT_VOLUME)),
            ("Office".to_owned(), Some(25)),
        ]
    );

    // Office has no Handoff; Kitchen has Alice's. Bob sets both.
    assert_eq!(
        api.set_group_volume(&setup.bobs, setup.office.uuid(), 35)
            .await,
        35
    );
    assert_eq!(
        api.set_group_volume(&setup.bobs, setup.kitchen.uuid(), 60)
            .await,
        60
    );
    assert_eq!(setup.household.volume(&setup.office), 35);
    assert_eq!(setup.household.volume(&setup.kitchen), 60);
    assert_eq!(
        volumes(api.groups(&setup.bobs).await),
        [
            ("Kitchen".to_owned(), Some(60)),
            ("Office".to_owned(), Some(35))
        ]
    );

    // Above 100 sets 100; 0 is silence.
    for (asked, set) in [(101, 100), (255, 100), (0, 0)] {
        assert_eq!(
            api.set_group_volume(&setup.bobs, setup.office.uuid(), asked)
                .await,
            set
        );
        assert_eq!(setup.household.volume(&setup.office), set);
    }

    // Not a volume at all (the call takes 0 to 255): refused by the call's decoding, before
    // the Bridge sees it.
    let authorization = format!("Bearer {}", setup.bobs.token.expose());
    for volume in [
        serde_json::json!(-1),
        serde_json::json!(256),
        serde_json::json!("loud"),
    ] {
        let response = api
            .post(
                "/api/groups/volume",
                Some(&authorization),
                serde_json::json!({"group": setup.office.uuid(), "volume": volume}),
            )
            .await;
        assert!(
            !response.status().is_success(),
            "{volume}: {}",
            response.status()
        );
    }
    assert_eq!(setup.household.volume(&setup.office), 0);

    // A Group whose coordinator doesn't tell its volume is listed without one.
    setup
        .household
        .set_fault(&setup.office, Fault::FailGroupVolume);
    assert_eq!(
        volumes(api.groups(&setup.bobs).await),
        [
            ("Kitchen".to_owned(), Some(60)),
            ("Office".to_owned(), None)
        ]
    );
}

#[tokio::test]
async fn a_group_without_a_handoff_or_playing_something_else_is_not_controlled() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let episode = setup.episode(0);

    // Office never had a Handoff.
    for control in HANDOFF_CONTROLS {
        assert_eq!(
            api.control_error(&setup.bobs, setup.office.uuid(), control)
                .await,
            NO_HANDOFF,
            "{control:?}"
        );
    }
    assert_eq!(setup.playback_controls(&setup.office), Vec::<String>::new());

    // Kitchen's Handoff was replaced by something played from the Sonos app.
    let handoff = setup.start(&episode, &setup.kitchen).await;
    setup.clock.advance(seconds(60));
    setup
        .household
        .play(&setup.kitchen, FakeTrack::new("Radio", seconds(3600)));
    let before = setup.playback_controls(&setup.kitchen);
    for control in HANDOFF_CONTROLS {
        assert_eq!(
            api.control_error(&setup.bobs, setup.kitchen.uuid(), control)
                .await,
            NO_HANDOFF,
            "{control:?}"
        );
    }
    assert_eq!(setup.playback_controls(&setup.kitchen), before);
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");
    // Nothing written, and the Handoff isn't ended here.
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);
    assert_eq!(api.handoffs(&setup.bobs).await, [handoff]);

    // A Group the stream of another Group's Handoff plays on is not that Handoff's.
    let other = setup.start(&setup.episode(1), &setup.office).await;
    let office_stream = setup.household.uri(&setup.office).unwrap();
    setup.household.play(
        &setup.kitchen,
        FakeTrack {
            uri: office_stream,
            ..FakeTrack::new("Episode 2", seconds(900))
        },
    );
    assert_eq!(
        api.control_error(&setup.bobs, setup.kitchen.uuid(), GroupControl::Pause)
            .await,
        NO_HANDOFF
    );
    assert_eq!(api.handoffs(&setup.bobs).await.len(), 2);
    assert_eq!(other.group, setup.office.uuid());
}

#[tokio::test]
async fn an_unknown_refusing_or_silent_group_fails_and_nothing_is_written() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.start(&setup.episode(1), &setup.office).await;
    setup.clock.advance(seconds(60));
    let every = [
        GroupControl::Pause,
        GroupControl::Resume,
        GroupControl::Skip(Skip::Back15),
        GroupControl::Volume(50),
    ];

    // No such Group, a member that doesn't coordinate, and no Group id at all.
    for group in ["RINCON_NOSUCH01400", setup.bath.uuid(), "../../etc"] {
        for control in every {
            assert_eq!(
                api.control_error(&setup.bobs, group, control).await,
                GROUP_NOT_FOUND,
                "{group} {control:?}"
            );
        }
    }

    // Kitchen refuses every control.
    setup
        .household
        .set_fault(&setup.kitchen, Fault::RefuseControl);
    for control in every {
        assert_eq!(
            api.control_error(&setup.bobs, setup.kitchen.uuid(), control)
                .await,
            GROUP_FAILED,
            "{control:?}"
        );
    }
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");
    assert_eq!(
        setup.household.volume(&setup.kitchen),
        sonos_fake::DEFAULT_VOLUME
    );
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);

    // Office stops answering; Kitchen still tells the topology.
    setup.household.set_fault(&setup.office, Fault::Hang);
    let start = Instant::now();
    assert_eq!(
        api.control_error(&setup.bobs, setup.office.uuid(), GroupControl::Pause)
            .await,
        GROUP_FAILED
    );
    let elapsed = start.elapsed();
    // Within the Bridge's limit for a control, and not much longer.
    assert!(elapsed < sonos::CONTROL_TIMEOUT + seconds(3), "{elapsed:?}");
    assert_eq!(setup.progress(&setup.alice, &setup.episode(1)).await, None);
    // Both Handoffs go on.
    assert_eq!(api.handoffs(&setup.bobs).await.len(), 2);
}

#[tokio::test]
async fn controls_need_a_session() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    let every = [
        GroupControl::Pause,
        GroupControl::Resume,
        GroupControl::Skip(Skip::Forward30),
        GroupControl::Volume(50),
    ];

    let before = setup.playback_controls(&setup.kitchen);
    api.logout(&setup.bobs).await;
    for control in every {
        assert_eq!(
            api.control_error_unauthenticated(setup.kitchen.uuid(), control)
                .await,
            SESSION_INVALID,
            "{control:?}"
        );
        assert_eq!(
            api.control_error(&setup.bobs, setup.kitchen.uuid(), control)
                .await,
            SESSION_INVALID,
            "{control:?}"
        );
    }
    assert_eq!(setup.playback_controls(&setup.kitchen), before);
    assert_eq!(
        setup.household.volume(&setup.kitchen),
        sonos_fake::DEFAULT_VOLUME
    );
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);
}

#[tokio::test]
async fn a_library_server_down_at_write_time_doesnt_undo_the_control() {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let api = setup.bridge.api();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.clock.advance(seconds(200));

    forwarder.stop().await;
    let start = Instant::now();
    // Without the Library Server, the Item's length is unknown too.
    assert_eq!(
        api.pause_group(&setup.bobs, setup.kitchen.uuid()).await,
        Playback {
            state: TransportState::Paused,
            position: Some(200.0),
            duration: None,
        }
    );
    assert!(start.elapsed() < seconds(5), "{:?}", start.elapsed());
    assert_eq!(
        setup.household.transport_state(&setup.kitchen),
        "PAUSED_PLAYBACK"
    );
    // A skip goes on without an end to stop at.
    assert_eq!(
        api.skip_group(&setup.bobs, setup.kitchen.uuid(), Skip::Forward30)
            .await
            .position,
        Some(230.0)
    );
    assert_eq!(setup.position(&setup.kitchen), seconds(230));

    forwarder.resume();
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);
    // The Handoff goes on, and the next control writes again.
    assert_eq!(api.handoffs(&setup.bobs).await.len(), 1);
    api.skip_group(&setup.bobs, setup.kitchen.uuid(), Skip::Back15)
        .await;
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(215.0));

    let output = setup.bridge.stop();
    let unwritten = output
        .iter()
        .filter(|event| event["level"] == "WARN" && event["message"] == "Progress not written")
        .count();
    assert_eq!(unwritten, 2);
    assert!(!output_contains(&output, &setup.alice.api_key.key));
}

#[tokio::test]
async fn a_starters_key_rejected_at_write_time_is_revoked_ending_the_handoff() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.clock.advance(seconds(60));

    setup.abs.delete_api_key(&setup.alice.api_key).await;

    // The Group paused all the same.
    assert_eq!(
        api.pause_group(&setup.bobs, setup.kitchen.uuid())
            .await
            .state,
        TransportState::Paused
    );
    assert_eq!(
        setup.household.transport_state(&setup.kitchen),
        "PAUSED_PLAYBACK"
    );
    // Alice's key is revoked: her Handoff and her session ended; Bob's session goes on.
    assert_eq!(api.handoffs(&setup.bobs).await, []);
    assert_eq!(api.current_user_error(&setup.alices).await, SESSION_INVALID);
    assert_eq!(
        api.control_error(&setup.bobs, setup.kitchen.uuid(), GroupControl::Resume)
            .await,
        NO_HANDOFF
    );
    assert_eq!(setup.progress(&setup.bob, &episode).await, None);
    assert!(!output_contains(
        &setup.bridge.stop(),
        &setup.alice.api_key.key
    ));
}

#[tokio::test]
async fn a_control_takes_turns_with_a_start_on_its_group() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let (first, second) = (setup.episode(0), setup.episode(1));
    setup.start(&first, &setup.office).await;
    // Office takes each URL a second late, so the pause comes while the next start runs.
    setup
        .household
        .set_fault(&setup.office, Fault::SlowPlayback(seconds(1)));
    let set_uri = "AVTransport#SetAVTransportURI".to_owned();
    let offered = || {
        let actions = setup.household.actions_of(&setup.office);
        actions.iter().filter(|action| **action == set_uri).count()
    };

    let (started, paused) = tokio::join!(
        api.start_handoff(&setup.alices, &second, setup.office.uuid()),
        async {
            let deadline = Instant::now() + seconds(10);
            while offered() < 2 {
                assert!(Instant::now() < deadline, "the second start never came");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            api.pause_group(&setup.bobs, setup.office.uuid()).await
        },
    );

    // The pause came after the start had the Group play, and paused the new Handoff.
    assert_eq!(started.item, second);
    assert_eq!(paused.state, TransportState::Paused);
    let order: Vec<String> = setup
        .household
        .actions_of(&setup.office)
        .into_iter()
        .filter(|action| *action == set_uri || PLAYBACK_CONTROLS.contains(&action.as_str()))
        .collect();
    assert_eq!(
        order,
        [
            set_uri.as_str(),
            "AVTransport#Play",
            set_uri.as_str(),
            "AVTransport#Play",
            "AVTransport#Pause",
        ]
    );
    assert_eq!(setup.progress(&setup.alice, &second).await, Some(0.0));
    assert_eq!(setup.progress(&setup.alice, &first).await, None);
}

/// Pins the wire shape the App decodes, independent of the shared Rust types.
#[tokio::test]
async fn control_wire_shape() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let kitchen = setup.kitchen.uuid();
    let handoff = setup.start(&setup.episode(0), &setup.kitchen).await;
    setup.clock.advance(seconds(42));

    let paused: serde_json::Value = api.control(&setup.bobs, kitchen, GroupControl::Pause).await;
    assert_eq!(
        paused,
        serde_json::json!({"state": "paused", "position": 42.0, "duration": 600.0})
    );
    let skipped: serde_json::Value = api
        .post(
            "/api/groups/skip",
            Some(&format!("Bearer {}", setup.bobs.token.expose())),
            serde_json::json!({"group": kitchen, "skip": "back15"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(skipped["position"], 27.0);
    let volume: serde_json::Value = api
        .control(&setup.bobs, kitchen, GroupControl::Volume(30))
        .await;
    assert_eq!(volume, serde_json::json!(30));

    let groups: serde_json::Value = api.get_as(&setup.bobs, "/api/groups", &[]).await;
    assert_eq!(groups[0]["name"], "Kitchen");
    assert_eq!(groups[0]["volume"], 30);
    assert_eq!(
        groups[0]["handoff"],
        serde_json::json!({
            "group": kitchen,
            "item": {
                "library_item": setup.abs.seeded().podcast,
                "episode": setup.abs.seeded().episodes[0],
            },
            "started_by": {
                "library_server": setup.abs.base_url(),
                "user_id": setup.alice.id,
                "username": setup.alice.username,
            },
            "started_at": handoff.started_at,
        })
    );
}
