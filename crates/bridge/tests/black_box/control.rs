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
    GROUP_FAILED, GROUP_NOT_FOUND, GroupControl, KEY_REJECTED, LENGTH_UNKNOWN,
    LIBRARY_SERVER_UNREACHABLE, NO_HANDOFF, SESSION_INVALID, TestBridge, home_with_sonos_toml,
    output_contains,
};
use test_support::eventually;

use crate::forwarder::Forwarder;
use crate::library_proxy::LibraryProxy;

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
        self.abs.seeded().episode(n)
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
        let progress = self.abs.item_progress(user, item).await;
        progress.map(|(current_time, _)| current_time)
    }

    /// Records `user`'s Progress in `item` in Audiobookshelf, `current_time` seconds in and not
    /// Finished, as their own player would.
    async fn set_progress(&self, user: &AbsUser, item: &ItemId, current_time: f64) {
        let abs = &self.abs;
        abs.set_item_progress(user, item, current_time, false).await;
    }

    /// How often `player` was sent SOAP action `action`.
    fn asked(&self, player: &PlayerId, action: &str) -> usize {
        self.household.asked(player, action)
    }

    /// Waits until `player` was sent SOAP action `action` more than `before` times, at most
    /// `within`.
    async fn wait_until_asked(
        &self,
        player: &PlayerId,
        action: &str,
        before: usize,
        within: Duration,
    ) {
        let what = format!("{action} never came");
        eventually(&what, within, async || self.asked(player, action) > before).await;
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

    // Alice's own player moves her Progress on meanwhile. Bob resumes; it moves on from where
    // it paused, and resuming writes nothing: her Progress stays as her player left it.
    let seeded = setup.abs.seeded();
    let (podcast, first) = (&seeded.podcast, Some(seeded.episodes[0].as_str()));
    setup
        .abs
        .set_progress(&setup.alice, podcast, first, 42.0, false)
        .await;
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
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(42.0));

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

/// Bob pauses the Handoff Alice started 130 seconds ago on Kitchen, a Group slow to act: once
/// the Pause reached the Group, `change` runs and the Group plays on one more second before
/// it pauses. Answers with what the pause answered.
async fn pause_while(setup: &Setup, episode: &ItemId, change: impl FnOnce()) -> Playback {
    setup.start(episode, &setup.kitchen).await;
    setup.clock.advance(seconds(130));
    setup.household.set_fault(
        &setup.kitchen,
        Fault::SlowTransport(Duration::from_millis(300)),
    );
    let pauses = || {
        let actions = setup.household.actions_of(&setup.kitchen).into_iter();
        actions
            .filter(|action| action == "AVTransport#Pause")
            .count()
    };
    let before = pauses();
    let (paused, ()) = tokio::join!(
        setup
            .bridge
            .api()
            .pause_group(&setup.bobs, setup.kitchen.uuid()),
        async {
            let deadline = Instant::now() + seconds(4);
            while pauses() == before {
                assert!(
                    Instant::now() < deadline,
                    "the Pause never reached the Group"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            change();
            setup.clock.advance(seconds(1));
        }
    );
    paused
}

#[tokio::test]
async fn a_pause_writes_where_the_group_paused() {
    let setup = setup().await;
    let episode = setup.episode(0);

    let paused = pause_while(&setup, &episode, || {}).await;

    assert_eq!(setup.position(&setup.kitchen), seconds(131));
    assert_eq!(paused, playback(TransportState::Paused, 131.0, 600.0));
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(131.0));
}

#[tokio::test]
async fn a_pause_that_lands_on_something_else_writes_where_the_handoff_was() {
    let setup = setup().await;
    let episode = setup.episode(0);

    // Made to play something else (in the Sonos app, say) while the Pause is on its way: the
    // Pause pauses that, a second in.
    let paused = pause_while(&setup, &episode, || {
        let track = FakeTrack::new("Radio", seconds(3600));
        setup.household.play(&setup.kitchen, track);
    })
    .await;

    assert_eq!(setup.position(&setup.kitchen), seconds(1));
    assert_eq!(paused.position, Some(130.0));
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(130.0));
}

#[tokio::test]
async fn a_pause_whose_group_then_tells_no_position_writes_where_it_was_before() {
    let setup = setup().await;
    let episode = setup.episode(0);

    let paused = pause_while(&setup, &episode, || {
        setup.household.set_fault(&setup.kitchen, Fault::NoPosition);
    })
    .await;

    assert_eq!(setup.position(&setup.kitchen), seconds(131));
    assert_eq!(paused.position, Some(130.0));
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(130.0));
}

#[tokio::test]
async fn a_pause_whose_group_then_fails_to_tell_its_playback_writes_where_it_was_before() {
    let setup = setup().await;
    let episode = setup.episode(0);

    let paused = pause_while(&setup, &episode, || {
        setup
            .household
            .set_fault(&setup.kitchen, Fault::FailTransportInfo);
    })
    .await;

    assert_eq!(setup.position(&setup.kitchen), seconds(131));
    assert_eq!(paused.position, Some(130.0));
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(130.0));
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
    // With the Item's length, as Audiobookshelf stores it.
    let seeded = setup.abs.seeded();
    let record = setup
        .abs
        .progress_record(&setup.alice, &seeded.podcast, Some(&seeded.episodes[0]))
        .await
        .unwrap();
    assert_eq!(record["duration"], 600.0);
    assert_eq!(record["progress"].as_f64().unwrap(), 85.0 / 600.0);
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
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let api = setup.bridge.api();
    let office = setup.office.uuid();
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
    setup.clock.advance(seconds(100));

    // With the Library Server down, "30 s forward" needs nothing from it: the Group skips,
    // and nothing is written.
    forwarder.stop().await;
    assert_eq!(
        api.skip_group(&setup.bobs, office, Skip::Forward30).await,
        playback(TransportState::Playing, 130.0, 700.0)
    );
    assert_eq!(setup.position(&setup.office), seconds(130));
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);
    forwarder.resume();

    setup.clock.advance(seconds(690 - 130));
    assert_eq!(
        api.skip_group(&setup.bobs, office, Skip::Forward30).await,
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

    // Kitchen refuses every control: those for a playing Group, then a resume once paused.
    setup
        .household
        .set_fault(&setup.kitchen, Fault::RefuseControl);
    for control in every.into_iter().filter(|c| *c != GroupControl::Resume) {
        assert_eq!(
            api.control_error(&setup.bobs, setup.kitchen.uuid(), control)
                .await,
            GROUP_FAILED,
            "{control:?}"
        );
    }
    // The volume's snapshot was refused, so the volume itself was never sent.
    let actions = setup.household.actions_of(&setup.kitchen);
    let sent = |action: &str| actions.iter().any(|sent| sent == action);
    assert!(sent("GroupRenderingControl#SnapshotGroupVolume"));
    assert!(!sent("GroupRenderingControl#SetGroupVolume"));
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");
    assert_eq!(
        setup.household.volume(&setup.kitchen),
        sonos_fake::DEFAULT_VOLUME
    );
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);
    setup.household.pause(&setup.kitchen);
    assert_eq!(
        api.control_error(&setup.bobs, setup.kitchen.uuid(), GroupControl::Resume)
            .await,
        GROUP_FAILED
    );
    assert_eq!(
        setup.household.transport_state(&setup.kitchen),
        "PAUSED_PLAYBACK"
    );

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
    let kitchen = setup.kitchen.uuid();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.clock.advance(seconds(200));

    forwarder.stop().await;
    let start = Instant::now();
    // Without the Library Server, the Item's length is unknown too.
    assert_eq!(
        api.pause_group(&setup.bobs, kitchen).await,
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
    // "30 s forward" needs the Item's end, which only the Library Server tells for a stream:
    // refused, and nothing sent to the Group.
    let before = setup.playback_controls(&setup.kitchen);
    assert_eq!(
        api.control_error(&setup.bobs, kitchen, GroupControl::Skip(Skip::Forward30))
            .await,
        LIBRARY_SERVER_UNREACHABLE
    );
    assert_eq!(setup.playback_controls(&setup.kitchen), before);
    assert_eq!(setup.position(&setup.kitchen), seconds(200));
    // "15 s back" needs no end.
    assert_eq!(
        api.skip_group(&setup.bobs, kitchen, Skip::Back15)
            .await
            .position,
        Some(185.0)
    );
    assert_eq!(setup.position(&setup.kitchen), seconds(185));

    forwarder.resume();
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);
    // The Handoff goes on, and the next control writes again.
    assert_eq!(api.handoffs(&setup.bobs).await.len(), 1);
    api.skip_group(&setup.bobs, kitchen, Skip::Back15).await;
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(170.0));

    let output = setup.bridge.stop();
    let unwritten = output
        .iter()
        .filter(|event| event["level"] == "WARN" && event["message"] == "Progress not written")
        .count();
    assert_eq!(unwritten, 2);
    assert!(!output_contains(&output, &setup.alice.api_key.key));
}

/// How long a control may take at most with a Library Server that doesn't answer: the
/// Bridge's 7 seconds for it, and a margin for the Group's quick answers. Below the 10 seconds
/// Audiobookshelf's client gives each request, so only the Bridge's limit keeps a control
/// within it.
const LIBRARY_SERVER_BOUND: Duration = Duration::from_millis(8500);

#[tokio::test]
async fn a_library_server_that_hangs_doesnt_hold_a_control_up() {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let api = setup.bridge.api();
    let kitchen = setup.kitchen.uuid();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.clock.advance(seconds(200));

    // It takes every request and never answers.
    forwarder.stall_after(0).await;
    let start = Instant::now();
    assert_eq!(
        api.pause_group(&setup.bobs, kitchen).await,
        Playback {
            state: TransportState::Paused,
            position: Some(200.0),
            duration: None,
        }
    );
    assert!(
        start.elapsed() < LIBRARY_SERVER_BOUND,
        "{:?}",
        start.elapsed()
    );
    assert_eq!(
        setup.household.transport_state(&setup.kitchen),
        "PAUSED_PLAYBACK"
    );
    let start = Instant::now();
    assert_eq!(
        api.skip_group(&setup.bobs, kitchen, Skip::Back15).await,
        Playback {
            state: TransportState::Paused,
            position: Some(185.0),
            duration: None,
        }
    );
    assert!(
        start.elapsed() < LIBRARY_SERVER_BOUND,
        "{:?}",
        start.elapsed()
    );
    assert_eq!(setup.position(&setup.kitchen), seconds(185));
    // Nothing written (read past the forwarder), and the Handoff goes on.
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);
    assert_eq!(api.handoffs(&setup.bobs).await.len(), 1);
}

#[tokio::test]
async fn a_progress_write_that_hangs_doesnt_hold_a_pause_up() {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let api = setup.bridge.api();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.clock.advance(seconds(200));

    // It tells whom the key acts for and the Item's length, but never answers a write.
    forwarder.stall_requests("PATCH /api/me/progress").await;
    let start = Instant::now();
    assert_eq!(
        api.pause_group(&setup.bobs, setup.kitchen.uuid()).await,
        playback(TransportState::Paused, 200.0, 600.0)
    );
    assert!(
        start.elapsed() < LIBRARY_SERVER_BOUND,
        "{:?}",
        start.elapsed()
    );
    assert_eq!(
        setup.household.transport_state(&setup.kitchen),
        "PAUSED_PLAYBACK"
    );
    // The write never reached Audiobookshelf; the Handoff goes on.
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);
    assert_eq!(api.handoffs(&setup.bobs).await.len(), 1);

    let output = setup.bridge.stop();
    let unconfirmed = output
        .iter()
        .filter(|event| {
            event["level"] == "WARN" && event["message"] == "Progress write not confirmed"
        })
        .count();
    assert_eq!(unconfirmed, 1);
}

#[tokio::test]
async fn a_slow_item_length_doesnt_keep_a_pause_from_writing_progress() {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let api = setup.bridge.api();
    let kitchen = setup.kitchen.uuid();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.clock.advance(seconds(200));

    // It tells whom the key acts for, but never the Item (and so its length).
    forwarder.stall_requests("GET /api/items/").await;
    let start = Instant::now();
    assert_eq!(
        api.pause_group(&setup.bobs, kitchen).await,
        Playback {
            state: TransportState::Paused,
            position: Some(200.0),
            duration: None,
        }
    );
    assert!(
        start.elapsed() < LIBRARY_SERVER_BOUND,
        "{:?}",
        start.elapsed()
    );
    // Written without the length.
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(200.0));

    // "30 s forward" can't do without the length: refused in time, nothing sent.
    let before = setup.playback_controls(&setup.kitchen);
    let start = Instant::now();
    assert_eq!(
        api.control_error(&setup.bobs, kitchen, GroupControl::Skip(Skip::Forward30))
            .await,
        LIBRARY_SERVER_UNREACHABLE
    );
    assert!(
        start.elapsed() < LIBRARY_SERVER_BOUND,
        "{:?}",
        start.elapsed()
    );
    assert_eq!(setup.playback_controls(&setup.kitchen), before);
    assert_eq!(setup.position(&setup.kitchen), seconds(200));
}

#[tokio::test]
async fn a_forward_skip_waiting_for_the_length_skips_from_where_the_group_is_then() {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let api = setup.bridge.api();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.clock.advance(seconds(200));

    // A slow Library Server: the Group plays on 20 seconds while the Bridge waits for the
    // Item's length, which the Group doesn't tell for a stream.
    forwarder.delay_by(seconds(2)).await;
    forwarder.record_requests().await;
    let (skipped, ()) = tokio::join!(
        api.skip_group(&setup.bobs, setup.kitchen.uuid(), Skip::Forward30),
        async {
            tokio::time::sleep(seconds(1)).await;
            setup.clock.advance(seconds(20));
        },
    );
    assert_eq!(skipped, playback(TransportState::Playing, 250.0, 600.0));
    assert_eq!(setup.position(&setup.kitchen), seconds(250));
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(250.0));

    // The length was read, and everything asked, with the Handoff's key (Alice's), never
    // with the key of Bob, who skipped.
    let requests = forwarder.requests();
    let item = format!("GET /api/items/{}?", episode.library_item);
    assert!(requests.contains(&item), "{requests}");
    assert!(requests.contains(&setup.alice.api_key.key));
    assert!(!requests.contains(&setup.bob.api_key.key));
}

#[tokio::test]
async fn a_forward_skip_on_an_item_of_unknown_length_is_refused() {
    let abs = Audiobookshelf::shared();
    let proxy = LibraryProxy::to(abs.base_url()).await;
    let setup = setup_on(Arc::clone(&abs), &proxy.url()).await;
    let api = setup.bridge.api();
    let kitchen = setup.kitchen.uuid();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.clock.advance(seconds(590));
    setup
        .abs
        .set_progress(
            &setup.alice,
            &episode.library_item,
            episode.episode.as_deref(),
            42.0,
            false,
        )
        .await;

    // The Group tells no length for a stream, and the Library Server knows none either: there
    // is no end to stop at, so "30 s forward" is refused, nothing sent, nothing written.
    proxy.hide_lengths();
    let before = setup.playback_controls(&setup.kitchen);
    assert_eq!(
        api.control_error(&setup.bobs, kitchen, GroupControl::Skip(Skip::Forward30))
            .await,
        LENGTH_UNKNOWN
    );
    assert_eq!(setup.playback_controls(&setup.kitchen), before);
    assert_eq!(setup.position(&setup.kitchen), seconds(590));
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(42.0));
    assert_eq!(setup.progress(&setup.bob, &episode).await, None);

    // "15 s back" needs no length.
    assert_eq!(
        api.skip_group(&setup.bobs, kitchen, Skip::Back15).await,
        Playback {
            state: TransportState::Playing,
            position: Some(575.0),
            duration: None,
        }
    );
    assert_eq!(setup.position(&setup.kitchen), seconds(575));
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(575.0));
}

#[tokio::test]
async fn a_starters_key_rejected_at_write_time_is_revoked_ending_the_handoff() {
    for control in [GroupControl::Pause, GroupControl::Skip(Skip::Back15)] {
        let setup = setup().await;
        let api = setup.bridge.api();
        let episode = setup.episode(0);
        setup.start(&episode, &setup.kitchen).await;
        // No request with Alice's key in flight while it is deleted (see `delete_api_key`).
        setup.household.fetched(&setup.kitchen).await;
        setup.clock.advance(seconds(60));

        setup.abs.delete_api_key(&setup.alice.api_key).await;

        // The Group did as asked all the same.
        let answered: Playback = api
            .control(&setup.bobs, setup.kitchen.uuid(), control)
            .await;
        if control == GroupControl::Pause {
            assert_eq!(answered.state, TransportState::Paused);
            assert_eq!(
                setup.household.transport_state(&setup.kitchen),
                "PAUSED_PLAYBACK"
            );
        } else {
            assert_eq!(answered.position, Some(45.0));
            assert_eq!(setup.position(&setup.kitchen), seconds(45));
        }
        // Alice's key is revoked: her Handoff and her session ended; Bob's session goes on.
        assert_eq!(api.handoffs(&setup.bobs).await, [], "{control:?}");
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

    // "30 s forward" asks the Library Server for the length before it seeks: the rejected
    // key is revoked then, and with the Handoff ended nothing is sent to the Group.
    let setup = setup().await;
    let api = setup.bridge.api();
    setup.start(&setup.episode(0), &setup.kitchen).await;
    setup.household.fetched(&setup.kitchen).await;
    setup.clock.advance(seconds(60));
    setup.abs.delete_api_key(&setup.alice.api_key).await;
    let before = setup.playback_controls(&setup.kitchen);
    assert_eq!(
        api.control_error(
            &setup.bobs,
            setup.kitchen.uuid(),
            GroupControl::Skip(Skip::Forward30)
        )
        .await,
        NO_HANDOFF
    );
    assert_eq!(setup.playback_controls(&setup.kitchen), before);
    assert_eq!(api.handoffs(&setup.bobs).await, []);
}

/// After Bob paused Alice's Handoff (`answered`): the Group paused, Alice's key is
/// revoked (her Handoff and session ended) and Bob's session goes on; nobody has Progress in
/// `episode`. Read from the Bridge's own storage only, so that no later check of Alice's key
/// with the Library Server can be what revoked it.
async fn assert_revoked_by(setup: &Setup, episode: &ItemId, answered: &Playback) {
    let api = setup.bridge.api();
    assert_eq!(answered.state, TransportState::Paused);
    assert_eq!(
        setup.household.transport_state(&setup.kitchen),
        "PAUSED_PLAYBACK"
    );
    assert_eq!(api.handoffs(&setup.bobs).await, []);
    assert_eq!(
        api.get_as_error(&setup.alices, "/api/handoffs", &[]).await,
        SESSION_INVALID
    );
    assert_eq!(setup.progress(&setup.alice, episode).await, None);
    assert_eq!(setup.progress(&setup.bob, episode).await, None);
}

#[tokio::test]
async fn a_rejected_length_lookup_revokes_the_key_while_whom_it_acts_for_is_not_told() {
    let abs = Audiobookshelf::shared();
    let proxy = LibraryProxy::to(abs.base_url()).await;
    let setup = setup_on(Arc::clone(&abs), &proxy.url()).await;
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.household.fetched(&setup.kitchen).await;
    setup.clock.advance(seconds(100));

    // The Group tells no length, so a pause asks the Library Server for the Item's: it rejects
    // the key at once, but never says whom the key acts for.
    proxy.reject_item_lookups();
    proxy.hang_identity();
    let answered = setup
        .bridge
        .api()
        .pause_group(&setup.bobs, setup.kitchen.uuid())
        .await;

    assert_eq!(answered.position, Some(100.0));
    assert_revoked_by(&setup, &episode, &answered).await;
}

#[tokio::test]
async fn a_rejected_progress_write_revokes_the_key_ending_the_handoff() {
    let abs = Audiobookshelf::shared();
    let proxy = LibraryProxy::to(abs.base_url()).await;
    let setup = setup_on(Arc::clone(&abs), &proxy.url()).await;
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.household.fetched(&setup.kitchen).await;
    setup.clock.advance(seconds(110));

    // Whom the key acts for and the Item's length are told; only the write is rejected.
    proxy.reject_progress_writes();
    let answered = setup
        .bridge
        .api()
        .pause_group(&setup.bobs, setup.kitchen.uuid())
        .await;

    assert_eq!(answered, playback(TransportState::Paused, 110.0, 600.0));
    assert_revoked_by(&setup, &episode, &answered).await;
}

#[tokio::test]
async fn a_starters_key_moved_to_another_user_is_revoked_and_writes_nothing() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    // No request with Alice's key in flight while it is moved (see `move_api_key`).
    setup.household.fetched(&setup.kitchen).await;
    setup.clock.advance(seconds(60));

    // An admin makes Alice's key act for Bob.
    let key = &setup.alice.api_key;
    setup.abs.move_api_key(key, &setup.bob.id).await;

    // The Group paused all the same.
    assert_eq!(
        api.pause_group(&setup.bobs, setup.kitchen.uuid())
            .await
            .state,
        TransportState::Paused
    );
    // The key is revoked: Alice's Handoff and session ended.
    assert_eq!(api.handoffs(&setup.bobs).await, []);
    assert_eq!(api.current_user_error(&setup.alices).await, SESSION_INVALID);
    // Neither Bob, whom the key acts for now, nor Alice got Progress.
    assert_eq!(setup.progress(&setup.bob, &episode).await, None);
    setup.abs.move_api_key(key, &setup.alice.id).await;
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);
}

#[tokio::test]
async fn a_group_already_as_asked_is_sent_nothing() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let kitchen = setup.kitchen.uuid();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.clock.advance(seconds(60));
    api.pause_group(&setup.bobs, kitchen).await;
    // Alice's own player moves her Progress on.
    let seeded = setup.abs.seeded();
    let (podcast, first) = (&seeded.podcast, Some(seeded.episodes[0].as_str()));
    setup
        .abs
        .set_progress(&setup.alice, podcast, first, 42.0, false)
        .await;

    // Paused already: a pause sends nothing and writes nothing.
    let before = setup.playback_controls(&setup.kitchen);
    assert_eq!(
        api.pause_group(&setup.bobs, kitchen).await,
        Playback {
            state: TransportState::Paused,
            position: Some(60.0),
            duration: None,
        }
    );
    assert_eq!(setup.playback_controls(&setup.kitchen), before);
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(42.0));

    // Playing already: likewise for a resume.
    api.resume_group(&setup.bobs, kitchen).await;
    setup.clock.advance(seconds(5));
    let before = setup.playback_controls(&setup.kitchen);
    assert_eq!(
        api.resume_group(&setup.bobs, kitchen).await,
        Playback {
            state: TransportState::Playing,
            position: Some(65.0),
            duration: None,
        }
    );
    assert_eq!(setup.playback_controls(&setup.kitchen), before);
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(42.0));
}

#[tokio::test]
async fn a_group_that_tells_no_position_is_not_skipped_and_its_pause_writes_nothing() {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let api = setup.bridge.api();
    let kitchen = setup.kitchen.uuid();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.clock.advance(seconds(60));
    setup.household.set_fault(&setup.kitchen, Fault::NoPosition);
    forwarder.record_requests().await;

    let before = setup.playback_controls(&setup.kitchen);
    for skip in [Skip::Back15, Skip::Forward30] {
        assert_eq!(
            api.control_error(&setup.bobs, kitchen, GroupControl::Skip(skip))
                .await,
            GROUP_FAILED,
            "{skip:?}"
        );
    }
    assert_eq!(setup.playback_controls(&setup.kitchen), before);

    assert_eq!(
        api.pause_group(&setup.bobs, kitchen).await,
        Playback {
            state: TransportState::Paused,
            position: None,
            duration: None,
        }
    );
    assert_eq!(
        setup.household.transport_state(&setup.kitchen),
        "PAUSED_PLAYBACK"
    );
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);
    // Without a position there is nothing to ask the Library Server: neither whom the key acts
    // for, nor the Item's length, nor a write (only the stream's audio passes, as the Group
    // plays it).
    let requests = forwarder.requests();
    assert!(!requests.contains("/api/me"), "{requests}");
    let item = format!("GET /api/items/{}?", episode.library_item);
    assert!(!requests.contains(&item), "{requests}");
}

#[tokio::test]
async fn a_group_that_hangs_on_a_control_after_telling_its_playback_fails_in_time() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.clock.advance(seconds(60));
    setup
        .household
        .set_fault(&setup.kitchen, Fault::HangControl);

    for control in [
        GroupControl::Pause,
        GroupControl::Skip(Skip::Back15),
        GroupControl::Volume(50),
    ] {
        let start = Instant::now();
        assert_eq!(
            api.control_error(&setup.bobs, setup.kitchen.uuid(), control)
                .await,
            GROUP_FAILED,
            "{control:?}"
        );
        let elapsed = start.elapsed();
        // One limit for all of a control's calls to the Group.
        assert!(
            elapsed < sonos::CONTROL_TIMEOUT + seconds(2),
            "{control:?} {elapsed:?}"
        );
    }
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");
    assert_eq!(
        setup.household.volume(&setup.kitchen),
        sonos_fake::DEFAULT_VOLUME
    );
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);
}

#[tokio::test]
async fn a_control_waits_for_its_turn_on_the_group_only_so_long() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let kitchen = setup.kitchen.uuid();
    setup.start(&setup.episode(0), &setup.kitchen).await;
    setup.clock.advance(seconds(60));
    // A pause that hangs holds the Group's turn for the Group's whole 4 seconds.
    setup
        .household
        .set_fault(&setup.kitchen, Fault::HangControl);

    let (first, (queued, waited)) = tokio::join!(
        api.control_error(&setup.bobs, kitchen, GroupControl::Pause),
        async {
            // Once the Group got the Pause, the pause holds the turn.
            let deadline = Instant::now() + seconds(3);
            while !setup
                .household
                .actions_of(&setup.kitchen)
                .iter()
                .any(|action| action == "AVTransport#Pause")
            {
                assert!(Instant::now() < deadline, "the Group got no Pause");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let start = Instant::now();
            let queued = tokio::join!(
                api.control_error(&setup.bobs, kitchen, GroupControl::Skip(Skip::Back15)),
                api.control_error(&setup.bobs, kitchen, GroupControl::Volume(50)),
            );
            (queued, start.elapsed())
        },
    );
    assert_eq!(first, GROUP_FAILED);
    // The others gave up after 3 seconds of waiting, before the pause let go of the turn.
    assert_eq!(queued, (GROUP_FAILED, GROUP_FAILED));
    assert!(
        seconds(3) <= waited && waited < Duration::from_millis(3900),
        "{waited:?}"
    );
    // And sent nothing.
    let actions = setup.household.actions_of(&setup.kitchen);
    let count = |action: &str| actions.iter().filter(|sent| *sent == action).count();
    assert_eq!(count("AVTransport#Pause"), 1);
    assert_eq!(count("AVTransport#Seek"), 0);
    assert_eq!(count("GroupRenderingControl#SnapshotGroupVolume"), 0);
    assert_eq!(count("GroupRenderingControl#SetGroupVolume"), 0);
}

#[tokio::test]
async fn a_controls_calls_to_the_group_share_one_time_limit() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.clock.advance(seconds(60));
    // Each call alone answers well within the 4 seconds a control has for the Group (a fresh
    // 4 seconds per call would pass each), but a forward skip's three (read, read again after
    // the length, seek) take 4.5 seconds together, so only one limit shared by all fails it.
    setup.household.set_fault(
        &setup.kitchen,
        Fault::SlowTransport(Duration::from_millis(1500)),
    );

    let start = Instant::now();
    assert_eq!(
        api.control_error(
            &setup.bobs,
            setup.kitchen.uuid(),
            GroupControl::Skip(Skip::Forward30)
        )
        .await,
        GROUP_FAILED
    );
    let elapsed = start.elapsed();
    assert!(elapsed < sonos::CONTROL_TIMEOUT + seconds(1), "{elapsed:?}");
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);
}

#[tokio::test]
async fn a_forward_skip_acts_only_if_the_group_still_plays_the_handoff_after_the_length() {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let api = setup.bridge.api();
    let episode = setup.episode(0);
    setup.start(&episode, &setup.kitchen).await;
    setup.clock.advance(seconds(60));
    // A slow Library Server: the Group changes while the Bridge waits for the Item's length.
    // Each skip on fresh connections, so none reuses one already through the delay.
    let skip_while = async |change: &dyn Fn()| {
        forwarder.delay_by(seconds(2)).await;
        let (skipped, ()) = tokio::join!(
            api.control_error(
                &setup.bobs,
                setup.kitchen.uuid(),
                GroupControl::Skip(Skip::Forward30)
            ),
            async {
                tokio::time::sleep(seconds(1)).await;
                change();
            },
        );
        skipped
    };
    let before = setup.playback_controls(&setup.kitchen);

    // It stops telling its playback.
    let failing = || {
        setup
            .household
            .set_fault(&setup.kitchen, Fault::FailTransportInfo);
    };
    assert_eq!(skip_while(&failing).await, GROUP_FAILED);
    setup.household.set_fault(&setup.kitchen, Fault::None);

    // It is made to play something else.
    let radio = || {
        let track = FakeTrack::new("Radio", seconds(3600));
        setup.household.play(&setup.kitchen, track);
    };
    assert_eq!(skip_while(&radio).await, NO_HANDOFF);

    // Neither was skipped or written; the Handoff isn't ended.
    assert_eq!(setup.playback_controls(&setup.kitchen), before);
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);
    assert_eq!(api.handoffs(&setup.bobs).await.len(), 1);
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
    // The replaced Handoff's Progress was written where Office was on it: still at 0.
    assert_eq!(setup.progress(&setup.alice, &first).await, Some(0.0));
}

#[tokio::test]
async fn a_start_from_the_start_waits_for_its_turn_on_the_group_only_so_long() {
    // The start gave up on its turn once only the Group's 4 seconds to play were left of its
    // 14, before the pause let go, and well within the App's 15.
    start_behind_a_slow_pause(Some(754.6), true, seconds(9)..seconds(11)).await;
}

// At Alice's Progress, or without any (which the start reads again after its wait, and may
// find), the start also leaves 2 seconds for that read and the Group's 4 to seek: it gives up
// on its turn once 2 + 4 + 4 are left of its 14.

#[tokio::test]
async fn a_start_at_progress_waits_for_its_turn_less() {
    start_behind_a_slow_pause(Some(754.6), false, seconds(3)..seconds(5)).await;
}

#[tokio::test]
async fn a_start_without_progress_waits_for_its_turn_less() {
    start_behind_a_slow_pause(None, false, seconds(3)..seconds(5)).await;
}

/// Has Alice start an episode, with `progress` in it (seconds) if any, `from_start` or not,
/// on Kitchen while Bob's pause holds the Group's turn longer than the start can wait, and
/// checks that it gave up on its turn after a wait within `waited`, storing and playing
/// nothing.
async fn start_behind_a_slow_pause(
    progress: Option<f64>,
    from_start: bool,
    waited: std::ops::Range<Duration>,
) {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let api = setup.bridge.api();
    let kitchen = setup.kitchen.uuid();
    let (first, second) = (setup.episode(0), setup.episode(1));
    if let Some(progress) = progress {
        setup.set_progress(&setup.alice, &second, progress).await;
    }
    let handoff = setup.start(&first, &setup.kitchen).await;
    setup.household.fetched(&setup.kitchen).await;
    setup.clock.advance(seconds(60));
    // A pause holds the Group's turn about 10.4 seconds: 3.4 for the Group's slow answers
    // (its playback, then the Pause), then the 7 the Library Server has for a write it never
    // answers.
    setup.household.set_fault(
        &setup.kitchen,
        Fault::SlowTransport(Duration::from_millis(1700)),
    );
    forwarder.stall_requests("PATCH /api/me/progress").await;
    let asked = |action: &str| setup.asked(&setup.kitchen, action);
    let read = "AVTransport#GetTransportInfo";
    let (read_before, set_uri_before) = (asked(read), asked("AVTransport#SetAVTransportURI"));

    let (paused, (started, elapsed)) = tokio::join!(api.pause_group(&setup.bobs, kitchen), async {
        // Once the Group is asked for its playback, the pause holds the turn.
        setup
            .wait_until_asked(&setup.kitchen, read, read_before, seconds(3))
            .await;
        let start = Instant::now();
        let started = if from_start {
            api.start_handoff_from_start_error(&setup.alices, &second, kitchen)
                .await
        } else {
            api.start_handoff_error(&setup.alices, &second, kitchen)
                .await
        };
        (started, start.elapsed())
    },);

    assert_eq!(started, GROUP_FAILED);
    assert!(waited.contains(&elapsed), "{elapsed:?}");
    assert_eq!(paused.state, TransportState::Paused);
    // It stored nothing and played nothing: the first Handoff goes on, and the Group was
    // given no other URL.
    assert_eq!(api.handoffs(&setup.bobs).await, [handoff]);
    assert_eq!(asked("AVTransport#SetAVTransportURI"), set_uri_before);
    assert!(output_contains(
        &setup.bridge.stop(),
        "a Handoff not started"
    ));
}

#[tokio::test]
async fn a_start_behind_a_pause_begins_at_the_progress_the_pause_wrote() {
    let setup = setup().await;
    let episode = setup.episode(1);
    setup.set_progress(&setup.alice, &episode, 300.0).await;
    setup.start(&episode, &setup.kitchen).await;
    setup.household.fetched(&setup.kitchen).await;
    assert_eq!(setup.position(&setup.kitchen), seconds(300));
    setup.clock.advance(seconds(60));

    // Alice starts the Item again while Bob's pause, which writes 360 seconds, holds the
    // Group's turn: the start begins there, not at the 300 it read before its turn.
    start_behind_a_pause(&setup, &episode, Start::AtProgress).await;

    assert_eq!(setup.position(&setup.kitchen), seconds(360));
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(360.0));
}

#[tokio::test]
async fn a_start_without_progress_behind_a_pause_that_writes_some_seeks_to_it() {
    let setup = setup().await;
    let episode = setup.episode(1);
    setup.start(&episode, &setup.kitchen).await;
    setup.household.fetched(&setup.kitchen).await;
    setup.clock.advance(seconds(60));

    // Alice has no Progress when she starts the Item again; Bob's pause, holding the turn,
    // writes her 60 seconds, and the start still has the time to seek there.
    start_behind_a_pause(&setup, &episode, Start::AtProgress).await;

    assert_eq!(setup.position(&setup.kitchen), seconds(60));
    assert_eq!(
        setup
            .playback_controls(&setup.kitchen)
            .last()
            .map(String::as_str),
        Some("AVTransport#Seek")
    );
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(60.0));
}

#[tokio::test]
async fn a_start_from_the_start_behind_a_pause_begins_at_0() {
    let setup = setup().await;
    let episode = setup.episode(1);
    setup.set_progress(&setup.alice, &episode, 300.0).await;
    setup.start(&episode, &setup.kitchen).await;
    setup.household.fetched(&setup.kitchen).await;
    setup.clock.advance(seconds(60));

    // Bob's pause writes Alice 360 seconds while her "Play from start" waits for its turn: it
    // still begins at 0, sending no Seek after its Play.
    start_behind_a_pause(&setup, &episode, Start::FromStart).await;

    assert_eq!(setup.position(&setup.kitchen), Duration::ZERO);
    assert_eq!(
        setup
            .playback_controls(&setup.kitchen)
            .last()
            .map(String::as_str),
        Some("AVTransport#Play")
    );
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(360.0));
}

#[tokio::test]
async fn a_start_resolving_while_a_pause_ends_begins_at_the_progress_the_pause_wrote() {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let episode = setup.episode(1);
    setup.set_progress(&setup.alice, &episode, 300.0).await;
    setup.start(&episode, &setup.kitchen).await;
    setup.household.fetched(&setup.kitchen).await;
    setup.clock.advance(seconds(60));
    forwarder.record_requests().await;
    // Alice's start reads her Progress (300) at once, but gets the Item only 3 seconds later,
    // after Bob's pause, which holds the turn about 2 seconds, wrote 360 and let go of it: it
    // finds the turn free, and still begins at 360.
    let item = format!("GET /api/items/{}?", episode.library_item);
    forwarder.delay_next_request(item, seconds(3));

    start_behind_a_pause(&setup, &episode, Start::AtProgress).await;

    assert_eq!(setup.position(&setup.kitchen), seconds(360));
    assert_eq!(setup.progress(&setup.alice, &episode).await, Some(360.0));
}

#[tokio::test]
async fn a_start_on_a_free_group_reads_the_users_progress_once() {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let episode = setup.episode(1);
    setup.set_progress(&setup.alice, &episode, 300.0).await;
    forwarder.record_requests().await;

    setup.start(&episode, &setup.kitchen).await;

    assert_eq!(setup.position(&setup.kitchen), seconds(300));
    assert_eq!(forwarder.requests().matches("GET /api/me ").count(), 1);
}

/// How Alice starts an Item in [`start_behind_a_pause`].
#[derive(Debug, Clone, Copy)]
enum Start {
    /// "Play on…": at her Progress.
    AtProgress,
    /// "Play from start".
    FromStart,
}

/// Has Bob pause Kitchen, which tells its position a second late, and Alice start `item` on
/// it as `start` says once the pause holds the Group's turn; checks that both succeeded, the
/// start after the pause.
async fn start_behind_a_pause(setup: &Setup, item: &ItemId, start: Start) {
    let api = setup.bridge.api();
    let kitchen = setup.kitchen.uuid();
    setup
        .household
        .set_fault(&setup.kitchen, Fault::SlowPosition(seconds(1)));
    let read = "AVTransport#GetTransportInfo";
    let read_before = setup.asked(&setup.kitchen, read);

    let (paused, started) = tokio::join!(api.pause_group(&setup.bobs, kitchen), async {
        // Once the Group is asked for its playback, the pause holds the turn.
        setup
            .wait_until_asked(&setup.kitchen, read, read_before, seconds(3))
            .await;
        match start {
            Start::AtProgress => setup.start(item, &setup.kitchen).await,
            Start::FromStart => {
                api.start_handoff_from_start(&setup.alices, item, kitchen)
                    .await
            }
        }
    });

    assert_eq!(paused.state, TransportState::Paused);
    assert_eq!(started.item, *item);
    let order = setup.playback_controls(&setup.kitchen);
    let last = |sent: &str| order.iter().rposition(|action| action == sent);
    let (paused_at, played_at) = (last("AVTransport#Pause"), last("AVTransport#Play"));
    assert!(
        paused_at.is_some_and(|paused_at| played_at.is_some_and(|played_at| paused_at < played_at)),
        "{order:?}"
    );
    setup.household.set_fault(&setup.kitchen, Fault::None);
}

#[tokio::test]
async fn a_start_whose_progress_isnt_read_in_time_after_its_turn_sends_nothing() {
    start_failing_its_second_read(SecondRead::Late).await;
}

#[tokio::test]
async fn a_start_whose_key_is_rejected_after_its_turn_is_revoked_and_sends_nothing() {
    start_failing_its_second_read(SecondRead::Rejected).await;
}

/// How a start's second read of the user's Progress, after its turn, fails.
#[derive(Debug, Clone, Copy)]
enum SecondRead {
    /// The Library Server never answers it.
    Late,
    /// The key was deleted meanwhile.
    Rejected,
}

/// Has Bob start an episode from the start on Kitchen, which takes the URL two seconds late,
/// and Alice start another, with Progress, while Bob's holds the Group's turn; once the Library
/// Server answered her resolve, her second read of her Progress fails as `read` says. Checks
/// that her start answered accordingly, a late read within its time once Bob's start let go of
/// the turn, and stored and sent nothing.
async fn start_failing_its_second_read(read: SecondRead) {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let setup = setup_on(Arc::clone(&abs), &forwarder.url()).await;
    let api = setup.bridge.api();
    let kitchen = setup.kitchen.uuid();
    let (bobs_episode, episode) = (setup.episode(0), setup.episode(1));
    setup.set_progress(&setup.alice, &episode, 300.0).await;
    setup
        .household
        .set_fault(&setup.kitchen, Fault::SlowPlayback(seconds(2)));
    let set_uri = "AVTransport#SetAVTransportURI";
    let played = "AVTransport#Play";

    let (bobs, (alices, answered_at, turn_free_at)) = tokio::join!(
        api.start_handoff_from_start(&setup.bobs, &bobs_episode, kitchen),
        async {
            setup
                .wait_until_asked(&setup.kitchen, set_uri, 0, seconds(5))
                .await;
            // Bob's start holds the turn; Alice's resolves, then waits for it.
            forwarder.record_requests().await;
            let failing = async {
                // Both her resolve's answers began, so the Library Server handled both: what
                // comes now meets only her second read.
                let deadline = Instant::now() + seconds(1);
                while forwarder.answered() < 2 {
                    assert!(Instant::now() < deadline, "Alice's start never resolved");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                match read {
                    SecondRead::Late => forwarder.stall_next_request("GET /api/me "),
                    SecondRead::Rejected => abs.delete_api_key(&setup.alice.api_key).await,
                }
                // Bob's start lets go of the turn once Kitchen was told to play.
                setup
                    .wait_until_asked(&setup.kitchen, played, 0, seconds(3))
                    .await;
                Instant::now()
            };
            let starting = async {
                let answered = api
                    .start_handoff_error(&setup.alices, &episode, kitchen)
                    .await;
                (answered, Instant::now())
            };
            let ((answered, answered_at), turn_free_at) = tokio::join!(starting, failing);
            (answered, answered_at, turn_free_at)
        },
    );

    match read {
        SecondRead::Late => {
            assert_eq!(alices, LIBRARY_SERVER_UNREACHABLE);
            // Within the read's 2 seconds, not the Library Server client's own 10.
            let took = answered_at - turn_free_at;
            assert!(
                (Duration::from_millis(1500)..Duration::from_millis(2500)).contains(&took),
                "{took:?}"
            );
        }
        SecondRead::Rejected => {
            assert_eq!(alices, KEY_REJECTED);
            // Her session went with the key.
            assert_eq!(api.current_user_error(&setup.alices).await, SESSION_INVALID);
        }
    }
    // Bob's Handoff goes on; Kitchen was given no other URL.
    assert_eq!(api.handoffs(&setup.bobs).await, [bobs]);
    assert_eq!(setup.asked(&setup.kitchen, set_uri), 1);
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
