//! Handoff supervision between App API calls: while a Group plays a Handoff, its Progress is
//! written for the starter about every 30 seconds; played to the end of its Item, it is
//! written Finished there and the Handoff ends; a start replacing a Handoff writes the
//! replaced one's Progress first. Against the seeded Audiobookshelf (read back with each
//! user's own key) and `sonos-fake` players, with the Bridge following the household's manual
//! clock through its test-only clock hook, so time moves only when a test advances it.
//!
//! A supervisor visits its Group one visit after another, so a read a test waits for comes
//! after any earlier one of the same supervisor: counting reads once an expected one arrived
//! shows whether others came before it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{ItemId, Session, Skip, TransportState};
use sonos::Sonos;
use sonos_fake::{FakeTrack, Fault, Household, PlayerId};

use test_support::audiobookshelf::{AbsUser, Audiobookshelf, EPISODE_DURATIONS};
use test_support::bridge::{
    GROUP_FAILED, SESSION_INVALID, TestBridge, clock_toml, home_with_sonos_toml,
};
use test_support::{WITHIN, eventually};

use crate::library_proxy::LibraryProxy;

/// The SOAP action a Bridge reads a Group's transport state with, as `sonos-fake` records it.
const READ: &str = "AVTransport#GetTransportInfo";

/// The SOAP action a Bridge reads a Group's position with, alongside [`READ`].
const READ_POSITION: &str = "AVTransport#GetPositionInfo";

/// How long a start may take in all, in the Bridge (`START_TIME`).
const START_TIME: Duration = Duration::from_secs(14);

/// A household of Kitchen and Office, a Bridge searching it and following its clock, with the
/// shared Audiobookshelf as its Library Server (or a proxy in front of it), and two users
/// logged in: Alice, who starts Handoffs, and Bob, who controls them or starts his own.
struct Setup {
    abs: Arc<Audiobookshelf>,
    household: Household,
    clock: clock::ManualClock,
    kitchen: PlayerId,
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

/// As [`setup`], with the Library Server at `url` (a proxy in front of `abs`).
async fn setup_on(abs: Arc<Audiobookshelf>, url: &str) -> Setup {
    let (household, clock) = Household::with_manual_clock();
    household.set_event_clock(Arc::new(clock.clone()));
    let kitchen = household.add_player("Kitchen");
    let office = household.add_player("Office");
    let config = format!(
        "{}{}",
        home_with_sonos_toml(url, household.ssdp()),
        clock_toml(household.clock_address())
    );
    let bridge = TestBridge::start(&config);
    let (alice, bob) = (abs.new_user().await, abs.new_user().await);
    let alices = bridge.api().login(url, &alice.api_key.key).await;
    let bobs = bridge.api().login(url, &bob.api_key.key).await;
    Setup {
        abs,
        household,
        clock,
        kitchen,
        office,
        bridge,
        alice,
        alices,
        bob,
        bobs,
    }
}

/// A [`LibraryProxy`] in front of the shared Audiobookshelf, and a [`Setup`] on it.
async fn setup_with_proxy() -> (LibraryProxy, Setup) {
    let abs = Audiobookshelf::shared();
    let proxy = LibraryProxy::to(abs.base_url()).await;
    let setup = setup_on(abs, &proxy.url()).await;
    (proxy, setup)
}

impl Setup {
    /// Episode `n` (0-based) of the seeded podcast: 600, 900 and 3960 seconds long.
    fn episode(&self, n: usize) -> ItemId {
        self.abs.seeded().episode(n)
    }

    /// `session`'s user starts `item` on Kitchen, which plays it to its end, `length` seconds
    /// in, if given; waits until Kitchen fetched the stream.
    async fn start(&self, session: &Session, item: &ItemId, length: Option<f64>) {
        if let Some(length) = length {
            let length = Duration::from_secs_f64(length);
            self.household.set_url_length(&self.kitchen, length);
        }
        let kitchen = self.kitchen.uuid();
        self.bridge
            .api()
            .start_handoff(session, item, kitchen)
            .await;
        self.household.fetched(&self.kitchen).await;
    }

    /// Moves time on by `seconds`, and waits until the Bridge has seen it.
    async fn advance(&self, seconds: u64) {
        self.clock.advance(Duration::from_secs(seconds));
        self.household.clock_followed().await;
    }

    /// Moves time on by `seconds`, and waits until the Bridge read Kitchen afterwards: both of
    /// its reads arrived, so Kitchen answered them as of then, whenever time moves on next.
    async fn advance_and_read(&self, seconds: u64) {
        let before = (self.asked(READ), self.asked(READ_POSITION));
        self.advance(seconds).await;
        let read = async || self.asked(READ) > before.0 && self.asked(READ_POSITION) > before.1;
        eventually("the Bridge never read Kitchen", WITHIN, read).await;
    }

    /// Moves time on by `seconds`, and waits until the Bridge read Kitchen afterwards and was
    /// done with what it read ([`Self::settle`]).
    async fn advance_and_settle(&self, seconds: u64) {
        self.advance_and_read(seconds).await;
        self.settle().await;
    }

    /// Moves time on by `seconds`, one second at a time, waiting each time until the Bridge
    /// read Kitchen afterwards: as real time passes near the end of an Item, where the Bridge
    /// reads the Group every second.
    async fn read_every_second(&self, seconds: u64) {
        for _ in 0..seconds {
            self.advance_and_read(1).await;
        }
    }

    /// Waits until whatever the Bridge does holding Kitchen's turn is done: Bob sets Kitchen's
    /// volume to what it is, which changes nothing, once he has the turn.
    async fn settle(&self) {
        let volume = self.household.volume(&self.kitchen);
        let kitchen = self.kitchen.uuid();
        let api = self.bridge.api();
        assert_eq!(
            api.set_group_volume(&self.bobs, kitchen, volume).await,
            volume
        );
    }

    /// Defence in depth that nothing supervises Kitchen any more: Bob starts a witness Handoff
    /// on Office, time moves on to its first tick, and once the witness's Progress is written
    /// and Kitchen's turn is free, Kitchen must not have been read since. It catches a Kitchen
    /// supervisor that reads Kitchen by that tick and before the witness's write lands; it
    /// doesn't prove there is none: a supervisor is a task of its own, which the witness's
    /// write and the turn don't order (one still checking a key before taking the turn could
    /// read later), and a supervisor of a Handoff no longer stored stops before reading.
    async fn assert_kitchen_unsupervised(&self) {
        let reads = self.asked(READ);
        let witness = self.episode(2);
        let office = self.office.uuid();
        let api = self.bridge.api();
        api.start_handoff(&self.bobs, &witness, office).await;
        self.household.fetched(&self.office).await;
        self.advance(30).await;
        self.wait_for_progress(&self.bob, &witness, (30.0, false))
            .await;
        self.settle().await;
        assert_eq!(self.asked(READ), reads);
    }

    /// `user`'s Progress in `item`, read from Audiobookshelf with their own key:
    /// `(currentTime, isFinished)`, if any.
    async fn progress(&self, user: &AbsUser, item: &ItemId) -> Option<(f64, bool)> {
        self.abs.item_progress(user, item).await
    }

    /// Records `user`'s Progress in `item` in Audiobookshelf, as their own player would.
    async fn set_progress(&self, user: &AbsUser, item: &ItemId, current_time: f64, done: bool) {
        let abs = &self.abs;
        abs.set_item_progress(user, item, current_time, done).await;
    }

    /// Waits until `user`'s Progress in `item` is `expected`.
    async fn wait_for_progress(&self, user: &AbsUser, item: &ItemId, expected: (f64, bool)) {
        let what = format!("Progress never became {expected:?}");
        eventually(&what, WITHIN, async || {
            self.progress(user, item).await == Some(expected)
        })
        .await;
    }

    /// How many Handoffs are listed.
    async fn handoffs(&self) -> usize {
        self.bridge.api().handoffs(&self.bobs).await.len()
    }

    /// Waits until no Handoff is listed.
    async fn wait_until_no_handoff(&self) {
        let ended = async || self.handoffs().await == 0;
        eventually("the Handoff never ended", WITHIN, ended).await;
    }

    /// How often Kitchen was sent SOAP action `action`.
    fn asked(&self, action: &str) -> usize {
        self.household.asked(&self.kitchen, action)
    }

    /// Bob pauses Kitchen.
    async fn pause(&self) {
        let kitchen = self.kitchen.uuid();
        let paused = self.bridge.api().pause_group(&self.bobs, kitchen).await;
        assert_eq!(paused.state, TransportState::Paused);
    }

    /// Bob resumes Kitchen.
    async fn resume(&self) {
        let kitchen = self.kitchen.uuid();
        let resumed = self.bridge.api().resume_group(&self.bobs, kitchen).await;
        assert_eq!(resumed.state, TransportState::Playing);
    }

    /// Alice's Handoff of episode 0 (600 seconds) on Kitchen, played to its end, where Kitchen
    /// stopped at 600 seconds, and its Finished write failed, the first of the `failures`
    /// Progress writes the Library Server fails from then on; 605 seconds in.
    async fn ended_with_finished_failing(&self, proxy: &LibraryProxy, failures: usize) {
        let episode = self.episode(0);
        self.start(&self.alices, &episode, Some(EPISODE_DURATIONS[0]))
            .await;
        self.advance(590).await;
        self.wait_for_progress(&self.alice, &episode, (590.0, false))
            .await;
        self.advance_and_read(5).await;
        proxy.fail_progress_writes(failures);
        self.read_every_second(5).await;
        let failed = async || proxy.progress_writes_failed() == 1;
        eventually("the Finished write never came", WITHIN, failed).await;
        self.advance(5).await;
        self.settle().await;
    }
}

#[tokio::test]
async fn a_playing_handoff_writes_the_starters_progress_every_30_seconds() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    setup.start(&setup.alices, &episode, None).await;
    let reads = setup.asked(READ);

    // Not before 30 seconds: then once, with a single read of Kitchen. The supervisor writes
    // one write after another, so once the one at 30 arrived, any earlier one had too.
    setup.advance(29).await;
    setup.settle().await;
    setup.advance(1).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (30.0, false))
        .await;
    setup.settle().await;
    assert_eq!(proxy.progress_writes(), 1);
    assert_eq!(setup.asked(READ), reads + 1);
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (60.0, false))
        .await;

    // Paused, nothing is written: Alice's own player moves her Progress meanwhile, and it
    // stays as that left it.
    setup.pause().await;
    setup
        .set_progress(&setup.alice, &episode, 42.0, false)
        .await;
    setup.advance_and_settle(60).await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((42.0, false))
    );
}

#[tokio::test]
async fn a_resume_from_the_app_has_the_next_tick_write() {
    let setup = setup().await;
    let episode = setup.episode(0);
    setup.start(&setup.alices, &episode, None).await;
    setup.advance(300).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (300.0, false))
        .await;

    // Paused, Kitchen is found so at the next tick; resumed from the App, it plays on from
    // 300 seconds, and the tick after has its Progress written.
    setup.pause().await;
    setup.advance_and_settle(30).await;
    setup.resume().await;
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (330.0, false))
        .await;
}

#[tokio::test]
async fn a_resume_from_outside_the_app_has_the_next_tick_write() {
    let setup = setup().await;
    let episode = setup.episode(0);
    setup.start(&setup.alices, &episode, None).await;
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (30.0, false))
        .await;

    // Paused from the App at 30 seconds, Kitchen is found so at the tick at 60; resumed from
    // the Sonos app, it plays on from 30 seconds, and the tick at 90 has its Progress
    // written: 60 seconds, where Kitchen is then (time stands still from there).
    setup.pause().await;
    setup.advance_and_settle(30).await;
    let sonos_app = sonos::Upnp::start(setup.household.ssdp(), Arc::new(clock::SystemClock));
    sonos_app
        .resume(setup.kitchen.uuid())
        .await
        .expect("resume from the Sonos app");
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (60.0, false))
        .await;
    assert_eq!(
        setup.household.position(&setup.kitchen),
        Duration::from_secs(60)
    );
}

#[tokio::test]
async fn a_skip_while_a_cadence_write_is_on_its_way_goes_after_it() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    setup.start(&setup.alices, &episode, None).await;

    // The tick at 30 seconds writes 30, held at the Library Server.
    let held = proxy.hold_next_progress_write();
    let writes = proxy.progress_writes();
    setup.advance(30).await;
    let arrived = async || proxy.progress_writes() > writes;
    eventually("the cadence write never came", WITHIN, arrived).await;

    // Bob skips 15 seconds back meanwhile. The held write is let through half a second after
    // his skip was sent (time for a skip that didn't wait for it to be done, well within the
    // time the Bridge gives that write and a skip's wait for its turn) and lands; the skip,
    // which waited for it, goes from 30 to 15 and writes that. Time stands still, so Kitchen
    // stays at 15.
    let api = setup.bridge.api();
    let back = api.skip_group(&setup.bobs, setup.kitchen.uuid(), Skip::Back15);
    let release = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        held.release().await;
    };
    let (skipped, ()) = tokio::join!(back, release);
    assert_eq!(skipped.position, Some(15.0));
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((15.0, false))
    );
    assert_eq!(
        setup.household.position(&setup.kitchen),
        Duration::from_secs(15)
    );
}

#[tokio::test]
async fn reaching_the_end_of_the_item_sets_finished_and_the_group_stops() {
    let setup = setup().await;
    let episode = setup.episode(0);
    let length = EPISODE_DURATIONS[0];
    setup.start(&setup.alices, &episode, Some(length)).await;
    setup.advance(590).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (590.0, false))
        .await;

    // Near the end, the Bridge reads Kitchen every second; it plays on to the end of the
    // episode and stops there.
    setup.advance_and_read(5).await;
    setup.read_every_second(5).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (length, true))
        .await;
    setup.wait_until_no_handoff().await;
    let groups = setup.bridge.api().groups(&setup.bobs).await;
    let kitchen = groups.iter().find(|group| group.name == "Kitchen").unwrap();
    assert_eq!(
        (kitchen.state, &kitchen.handoff),
        (Some(TransportState::Stopped), &None)
    );
    // Nothing else was started on it, and nothing supervises it any more.
    assert_eq!(setup.asked("AVTransport#SetAVTransportURI"), 1);
    assert_eq!(setup.asked("AVTransport#Play"), 1);
    setup.assert_kitchen_unsupervised().await;
    assert_eq!(setup.household.transport_state(&setup.kitchen), "STOPPED");
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((length, true))
    );
}

#[tokio::test]
async fn a_skip_to_the_end_of_the_item_sets_finished_before_the_next_tick() {
    let setup = setup().await;
    let episode = setup.episode(0);
    let length = EPISODE_DURATIONS[0];
    setup.start(&setup.alices, &episode, Some(length)).await;
    setup.advance(570).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (570.0, false))
        .await;

    // Bob skips Kitchen to the end of the episode, where it stops; the Bridge, told so, finds
    // it stopped there within seconds, well before the next tick at 600.
    let kitchen = setup.kitchen.uuid();
    let skipped = setup
        .bridge
        .api()
        .skip_group(&setup.bobs, kitchen, Skip::Forward30)
        .await;
    assert_eq!(skipped.position, Some(length));
    for _ in 0..5 {
        setup.advance(1).await;
    }
    setup
        .wait_for_progress(&setup.alice, &episode, (length, true))
        .await;
    setup.wait_until_no_handoff().await;
}

#[tokio::test]
async fn an_item_whose_length_only_the_group_tells_ends_at_its_end() {
    let (proxy, setup) = setup_with_proxy().await;
    proxy.hide_lengths();
    let episode = setup.episode(0);
    let length = EPISODE_DURATIONS[0];
    setup.start(&setup.alices, &episode, Some(length)).await;
    setup.advance(590).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (590.0, false))
        .await;

    setup.advance_and_read(5).await;
    setup.read_every_second(5).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (length, true))
        .await;
    setup.wait_until_no_handoff().await;
}

#[tokio::test]
async fn a_handoff_started_mid_item_writes_absolute_positions_and_finishes_at_the_end() {
    let setup = setup().await;
    let episode = setup.episode(0);
    let length = EPISODE_DURATIONS[0];
    setup
        .set_progress(&setup.alice, &episode, 300.0, false)
        .await;

    // It starts at her Progress, so its first tick writes 30 seconds further on.
    setup.start(&setup.alices, &episode, Some(length)).await;
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (330.0, false))
        .await;

    // 300 seconds after the start, Kitchen reaches the end of the episode.
    setup.advance_and_settle(265).await;
    setup.read_every_second(5).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (length, true))
        .await;
    setup.wait_until_no_handoff().await;
    assert_eq!(setup.household.transport_state(&setup.kitchen), "STOPPED");
}

#[tokio::test]
async fn a_group_stopped_short_of_the_end_is_not_finished() {
    let setup = setup().await;
    let episode = setup.episode(0);
    setup
        .start(&setup.alices, &episode, Some(EPISODE_DURATIONS[0]))
        .await;
    setup.advance(575).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (575.0, false))
        .await;

    // Stopped from the Sonos app 25 seconds before the end; the Bridge next reads Kitchen
    // after the time it would have reached the end.
    setup.household.stop(&setup.kitchen);
    setup.advance_and_settle(30).await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((575.0, false))
    );
    assert_eq!(setup.handoffs().await, 1);
}

#[tokio::test]
async fn a_group_stopped_five_seconds_before_the_end_is_not_finished() {
    let setup = setup().await;
    let episode = setup.episode(0);
    setup
        .start(&setup.alices, &episode, Some(EPISODE_DURATIONS[0]))
        .await;
    setup.advance(595).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (595.0, false))
        .await;

    // Stopped from the Sonos app 5 seconds before the end, where the Bridge reads Kitchen
    // every second: found stopped a second later, it was not played to the end.
    setup.household.stop(&setup.kitchen);
    setup.advance_and_settle(1).await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((595.0, false))
    );
    assert_eq!(setup.handoffs().await, 1);

    // Past the end, the next tick finds it stopped all the same.
    setup.advance_and_settle(30).await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((595.0, false))
    );
    assert_eq!(setup.handoffs().await, 1);
}

#[tokio::test]
async fn a_group_stopped_three_seconds_before_the_end_is_not_finished() {
    let setup = setup().await;
    let episode = setup.episode(0);
    setup
        .start(&setup.alices, &episode, Some(EPISODE_DURATIONS[0]))
        .await;
    setup.advance(590).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (590.0, false))
        .await;

    // Read every second from 595; stopped from the Sonos app at 597, found stopped at 598.
    setup.advance_and_read(5).await;
    setup.read_every_second(2).await;
    setup.household.stop(&setup.kitchen);
    setup.advance_and_settle(1).await;
    setup.advance_and_settle(30).await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((590.0, false))
    );
    assert_eq!(setup.handoffs().await, 1);
}

#[tokio::test]
async fn a_group_stopped_after_failed_reads_near_the_end_is_not_finished() {
    let setup = setup().await;
    let episode = setup.episode(0);
    setup
        .start(&setup.alices, &episode, Some(EPISODE_DURATIONS[0]))
        .await;
    setup.advance(590).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (590.0, false))
        .await;

    // Read playing at 595 and 596; Kitchen fails to tell what it plays at 597 and 598, and is
    // stopped from the Sonos app at 598; found stopped at 599. Of the 3 seconds since it was
    // last seen playing, at most 2 count as played: 596 + 2, plus a second of position
    // granularity, falls short of the end at 600.
    setup.advance_and_read(5).await;
    setup.read_every_second(1).await;
    setup
        .household
        .set_fault(&setup.kitchen, Fault::FailTransportInfo);
    setup.read_every_second(2).await;
    setup.household.stop(&setup.kitchen);
    setup.household.set_fault(&setup.kitchen, Fault::None);
    setup.advance_and_settle(1).await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((590.0, false))
    );
    assert_eq!(setup.handoffs().await, 1);

    // Past the end, the next tick finds it stopped all the same.
    setup.advance_and_settle(30).await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((590.0, false))
    );
    assert_eq!(setup.handoffs().await, 1);
}

#[tokio::test]
async fn a_group_played_to_its_end_after_failed_reads_near_it_is_finished_at_once() {
    let setup = setup().await;
    let episode = setup.episode(0);
    let length = EPISODE_DURATIONS[0];
    // Started 310 seconds in, Kitchen reaches the end 290 seconds later, between two ticks.
    setup
        .set_progress(&setup.alice, &episode, 310.0, false)
        .await;
    setup.start(&setup.alices, &episode, Some(length)).await;
    setup.advance(270).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (580.0, false))
        .await;

    // Read every second from 595 to 597; Kitchen fails to tell what it plays at 598 and 599,
    // then plays on to the end at 600 and stops there: Finished is written then, not at the
    // next tick.
    setup.advance_and_read(15).await;
    setup.read_every_second(2).await;
    setup
        .household
        .set_fault(&setup.kitchen, Fault::FailTransportInfo);
    setup.read_every_second(2).await;
    setup.household.set_fault(&setup.kitchen, Fault::None);
    setup.advance(1).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (length, true))
        .await;
    setup.wait_until_no_handoff().await;
    assert_eq!(setup.household.transport_state(&setup.kitchen), "STOPPED");
}

#[tokio::test]
async fn near_the_end_the_group_is_read_without_asking_whom_the_key_acts_for_until_finished_is_due()
{
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    let length = EPISODE_DURATIONS[0];
    // Started 310 seconds in, Kitchen reaches the end 290 seconds later, between two ticks.
    setup
        .set_progress(&setup.alice, &episode, 310.0, false)
        .await;
    setup.start(&setup.alices, &episode, Some(length)).await;
    setup.advance(270).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (580.0, false))
        .await;
    let asked = proxy.identity_requests();

    // Read every second from 595 to 599 with no write due: the Library Server isn't asked
    // whom the key acts for, so the reads stay a second apart however slow it is.
    setup.advance_and_read(15).await;
    setup.read_every_second(4).await;
    assert_eq!(proxy.identity_requests(), asked);

    // Found stopped at its end at 600, the Finished write is due: asked once, for that write
    // (the key check that comes before each write), then nothing more is written.
    setup.advance(1).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (length, true))
        .await;
    assert_eq!(proxy.identity_requests(), asked + 1);
    setup.wait_until_no_handoff().await;
}

#[tokio::test]
async fn a_tick_among_the_reads_near_the_end_asks_nothing_so_a_slow_library_server_loses_no_end() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    let length = EPISODE_DURATIONS[0];
    // Started 27 seconds in, Kitchen is 3 seconds from the end at the tick at 570.
    setup
        .set_progress(&setup.alice, &episode, 27.0, false)
        .await;
    setup.start(&setup.alices, &episode, Some(length)).await;
    setup.advance(568).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (595.0, false))
        .await;
    // The read near the end comes at 569.5, so the next one comes at 570.5, after the tick
    // at 570: reads and ticks off the same second, as on a real clock.
    let before = (setup.asked(READ), setup.asked(READ_POSITION));
    setup.clock.advance(Duration::from_millis(1500));
    setup.household.clock_followed().await;
    let read = async || setup.asked(READ) > before.0 && setup.asked(READ_POSITION) > before.1;
    eventually("the Bridge never read Kitchen at 569.5", WITHIN, read).await;
    setup.settle().await;

    // The Library Server takes 2.5 seconds to tell whom a key acts for. The tick at 570 is
    // one of the reads near the end: at 570.5 the Bridge doesn't ask first, it reads Kitchen,
    // so time moved on 3 seconds from there finds Kitchen stopped at its end, close enough to
    // that read to count as played to it.
    let before = (
        proxy.identity_requests(),
        setup.asked(READ),
        setup.asked(READ_POSITION),
    );
    proxy.delay_next_identity(Duration::from_millis(2500));
    setup.advance(1).await;
    let acted = async || {
        proxy.identity_requests() > before.0
            || (setup.asked(READ) > before.1 && setup.asked(READ_POSITION) > before.2)
    };
    eventually("the Bridge did nothing at the tick", WITHIN, acted).await;
    assert_eq!(proxy.identity_requests(), before.0, "asked at the tick");
    // The Finished write's own key check comes at full speed.
    proxy.delay_next_identity(Duration::ZERO);
    setup.advance(3).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (length, true))
        .await;
    setup.wait_until_no_handoff().await;
    assert_eq!(setup.household.transport_state(&setup.kitchen), "STOPPED");
}

#[tokio::test]
async fn an_outside_resume_first_seen_at_a_tick_near_the_end_keeps_the_end() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    let length = EPISODE_DURATIONS[0];
    // Started 536 seconds in: the tick at 30 writes 566, then Bob pauses and the Sonos app
    // resumes Kitchen, which the Bridge doesn't see.
    setup
        .set_progress(&setup.alice, &episode, 536.0, false)
        .await;
    setup.start(&setup.alices, &episode, Some(length)).await;
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (566.0, false))
        .await;
    setup.pause().await;
    let sonos_app = sonos::Upnp::start(setup.household.ssdp(), Arc::new(clock::SystemClock));
    sonos_app
        .resume(setup.kitchen.uuid())
        .await
        .expect("resume");
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");

    // The Library Server takes 2.5 seconds to tell whom a key acts for. The tick at 60 first
    // sees Kitchen playing, at 596, 4 seconds from the end: the reads near the end have begun,
    // so the visit right after it reads Kitchen without asking first, and writes nothing.
    let asked = proxy.identity_requests();
    let before = (setup.asked(READ), setup.asked(READ_POSITION));
    proxy.delay_next_identity(Duration::from_millis(2500));
    setup.advance(30).await;
    let both =
        async || setup.asked(READ) >= before.0 + 2 && setup.asked(READ_POSITION) >= before.1 + 2;
    eventually(
        "the tick and the visit after it never read Kitchen",
        WITHIN,
        both,
    )
    .await;
    assert_eq!(proxy.identity_requests(), asked, "asked near the end");
    // The Finished write's own key check comes at full speed.
    proxy.delay_next_identity(Duration::ZERO);
    setup.settle().await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((566.0, false))
    );

    // Read every second up to 599, then found stopped at its end.
    setup.read_every_second(3).await;
    setup.advance(1).await;
    assert_eq!(setup.household.transport_state(&setup.kitchen), "STOPPED");
    setup
        .wait_for_progress(&setup.alice, &episode, (length, true))
        .await;
    setup.wait_until_no_handoff().await;
}

#[tokio::test]
async fn the_ticks_write_again_once_the_group_plays_on_past_where_the_end_was_expected() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    // Audiobookshelf holds the episode as 600 seconds long; Kitchen plays it for 700.
    setup
        .set_progress(&setup.alice, &episode, 540.0, false)
        .await;
    setup.start(&setup.alices, &episode, Some(700.0)).await;
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (570.0, false))
        .await;
    setup.settle().await;
    let writes = proxy.progress_writes();

    // Read every second from 595 to 606, the tick at 600 among them: nothing written.
    setup.advance_and_read(25).await;
    setup.read_every_second(11).await;
    setup.settle().await;
    assert_eq!(proxy.progress_writes(), writes);
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");

    // Seen playing more than 5 seconds past where the end was expected, the reads near the
    // end stop, and the tick at 90 writes again.
    setup.advance(24).await;
    let wrote = async || proxy.progress_writes() > writes;
    eventually("the tick at 90 never wrote", WITHIN, wrote).await;
    // Kitchen at 630, past the 600 seconds Audiobookshelf holds: Audiobookshelf 2.37.2 marks
    // the Item Finished itself for a position past its length (see the README), though the
    // Bridge's write leaves Finished unset.
    setup
        .wait_for_progress(&setup.alice, &episode, (630.0, true))
        .await;
}

#[tokio::test]
async fn a_tick_passing_while_a_near_end_visit_waits_for_the_turn_asks_nothing() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    setup
        .set_progress(&setup.alice, &episode, 26.0, false)
        .await;
    setup
        .start(&setup.alices, &episode, Some(EPISODE_DURATIONS[0]))
        .await;
    // The catch-up visit for the tick at 30, at 567.9, writes 593; the reads near the end then
    // come at 569.9, 570.9, and so on.
    setup.clock.advance(Duration::from_millis(567_900));
    setup.household.clock_followed().await;
    setup
        .wait_for_progress(&setup.alice, &episode, (593.0, false))
        .await;
    setup.settle().await;

    // Bob pauses with his write held, so he holds Kitchen's turn; the Sonos app resumes at
    // once. The read near the end at 569.9 waits for the turn while the tick at 570 passes,
    // and reads at 570.1 (Kitchen at 596.1), after the tick.
    let held = proxy.hold_next_progress_write();
    let writes = proxy.progress_writes();
    let api = setup.bridge.api();
    let pause = api.pause_group(&setup.bobs, setup.kitchen.uuid());
    let rest = async {
        let pause_write = async || proxy.progress_writes() > writes;
        eventually("the pause's write never came", WITHIN, pause_write).await;
        let sonos_app = sonos::Upnp::start(setup.household.ssdp(), Arc::new(clock::SystemClock));
        sonos_app
            .resume(setup.kitchen.uuid())
            .await
            .expect("resume");
        setup.clock.advance(Duration::from_millis(2000));
        setup.household.clock_followed().await;
        // Real time for the visit at 569.9 to begin and wait for the turn.
        tokio::time::sleep(Duration::from_millis(300)).await;
        setup.clock.advance(Duration::from_millis(200));
        setup.household.clock_followed().await;
        let reads = setup.asked(READ_POSITION);
        held.release().await;
        let read = async || setup.asked(READ_POSITION) > reads;
        eventually("the waiting visit never read Kitchen", WITHIN, read).await;
    };
    let (_, ()) = tokio::join!(pause, rest);
    setup.settle().await;

    // The Library Server takes 1.5 seconds to tell whom a key acts for. The next visit, at
    // 570.9 with the tick at 570 due, is among the reads near the end for Kitchen as last
    // seen (596.1 at 570.1, end expected at 574.1): it reads without asking first.
    let before = (
        proxy.identity_requests(),
        setup.asked(READ),
        setup.asked(READ_POSITION),
    );
    proxy.delay_next_identity(Duration::from_millis(1500));
    setup.clock.advance(Duration::from_millis(800));
    setup.household.clock_followed().await;
    let acted = async || {
        proxy.identity_requests() > before.0
            || (setup.asked(READ) > before.1 && setup.asked(READ_POSITION) > before.2)
    };
    eventually("the Bridge did nothing at 570.9", WITHIN, acted).await;
    assert_eq!(
        proxy.identity_requests(),
        before.0,
        "asked at a tick among the reads near the end"
    );
    proxy.delay_next_identity(Duration::ZERO);
}

#[tokio::test]
async fn a_start_replacing_a_handoff_paused_then_stopped_near_the_end_writes_no_finished() {
    let setup = setup().await;
    let first = setup.episode(0);
    alice_paused_then_stopped_a_second_before_the_end(&setup).await;

    setup.start(&setup.bobs, &setup.episode(1), None).await;
    assert_eq!(
        setup.progress(&setup.alice, &first).await,
        Some((599.0, false))
    );
    assert_bobs_handoff_listed(&setup).await;
}

#[tokio::test]
async fn a_group_paused_then_stopped_near_the_end_is_not_finished() {
    let setup = setup().await;
    let first = setup.episode(0);
    alice_paused_then_stopped_a_second_before_the_end(&setup).await;

    // Past the end, the next tick finds Kitchen stopped, last seen paused.
    setup.advance_and_settle(30).await;
    assert_eq!(
        setup.progress(&setup.alice, &first).await,
        Some((599.0, false))
    );
    assert_eq!(setup.handoffs().await, 1);
}

#[tokio::test]
async fn pausing_just_before_the_end_does_not_set_finished() {
    let setup = setup().await;
    let episode = setup.episode(0);
    setup
        .start(&setup.alices, &episode, Some(EPISODE_DURATIONS[0]))
        .await;
    setup.advance(595).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (595.0, false))
        .await;

    // Five seconds before the end.
    setup.pause().await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((595.0, false))
    );
    setup.advance_and_settle(60).await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((595.0, false))
    );
    assert_eq!(
        setup.household.transport_state(&setup.kitchen),
        "PAUSED_PLAYBACK"
    );
    assert_eq!(setup.handoffs().await, 1);
}

#[tokio::test]
async fn a_group_paused_near_the_end_then_resumed_finishes_at_the_end() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    let length = EPISODE_DURATIONS[0];
    setup.start(&setup.alices, &episode, Some(length)).await;
    setup.advance(595).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (595.0, false))
        .await;

    // Paused 5 seconds before the end for 30 seconds: nothing is written meanwhile.
    setup.pause().await;
    let writes = proxy.progress_writes();
    setup.advance_and_settle(30).await;
    assert_eq!(proxy.progress_writes(), writes);
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((595.0, false))
    );

    // Resumed from the App, it plays on to the end of the episode.
    setup.resume().await;
    setup.read_every_second(5).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (length, true))
        .await;
    setup.wait_until_no_handoff().await;
    assert_eq!(setup.household.transport_state(&setup.kitchen), "STOPPED");
}

#[tokio::test]
async fn a_finished_write_that_fails_is_tried_again() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    let length = EPISODE_DURATIONS[0];

    // The Library Server fails the Finished write: the Handoff stays for another try.
    setup.ended_with_finished_failing(&proxy, 1).await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((590.0, false))
    );
    assert_eq!(setup.handoffs().await, 1);

    // The next try lands.
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (length, true))
        .await;
    setup.wait_until_no_handoff().await;
}

#[tokio::test]
async fn a_finished_write_that_keeps_failing_ends_the_handoff_after_three_tries() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);

    setup.ended_with_finished_failing(&proxy, 4).await;
    for tries in 1..=2 {
        let failed = async || proxy.progress_writes_failed() == tries;
        eventually("a Finished write never came", WITHIN, failed).await;
        setup.settle().await;
        assert_eq!(setup.handoffs().await, 1);
        setup.advance(30).await;
    }
    setup.wait_until_no_handoff().await;
    assert_eq!(proxy.progress_writes_failed(), 3);
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((590.0, false))
    );
}

#[tokio::test]
async fn a_group_resumed_from_the_app_after_a_failed_finished_write_plays_on_unfinished() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    setup.ended_with_finished_failing(&proxy, 1).await;

    // Bob resumes Kitchen, which plays the episode again from its start at 605 seconds: the
    // next tick writes where it is, not Finished, and the Handoff goes on.
    setup.resume().await;
    setup.advance_and_settle(25).await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((25.0, false))
    );
    assert_eq!(setup.handoffs().await, 1);
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");
}

#[tokio::test]
async fn a_group_playing_again_after_a_failed_finished_write_is_not_finished() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    setup.ended_with_finished_failing(&proxy, 1).await;

    // Played again from outside the App: the Bridge finds it playing at the next tick, writes
    // where it is, not Finished, and the Handoff goes on.
    setup
        .household
        .set_transport_state(&setup.kitchen, "PLAYING");
    setup.advance_and_settle(25).await;
    let progress = setup.progress(&setup.alice, &episode).await;
    assert!(
        matches!(progress, Some((position, false)) if position < 590.0),
        "{progress:?}"
    );
    assert_eq!(setup.handoffs().await, 1);
}

#[tokio::test]
async fn a_group_resumed_from_the_app_then_stopped_after_a_failed_finished_write_is_not_finished() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    setup.ended_with_finished_failing(&proxy, 1).await;

    // Bob resumes Kitchen, which plays the episode again from its start, and before the next
    // tick it is stopped from the Sonos app: stopped short of the end, so nothing is written
    // and the Handoff stays.
    setup.resume().await;
    setup.household.stop(&setup.kitchen);
    setup.advance_and_settle(25).await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((590.0, false))
    );
    assert_eq!(setup.handoffs().await, 1);
}

#[tokio::test]
async fn a_key_rejected_on_a_finished_retry_is_revoked_ending_the_handoff() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    setup.ended_with_finished_failing(&proxy, 1).await;

    proxy.reject_progress_writes();
    setup.advance(30).await;
    setup.wait_until_no_handoff().await;
    assert_eq!(
        setup.bridge.api().current_user_error(&setup.alices).await,
        SESSION_INVALID
    );
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((590.0, false))
    );
}

#[tokio::test]
async fn a_start_replacing_a_handoff_whose_finished_write_failed_writes_it() {
    let (proxy, setup) = setup_with_proxy().await;
    setup.ended_with_finished_failing(&proxy, 1).await;

    setup.start(&setup.bobs, &setup.episode(1), None).await;
    assert_eq!(
        setup.progress(&setup.alice, &setup.episode(0)).await,
        Some((EPISODE_DURATIONS[0], true))
    );
    assert_eq!(setup.progress(&setup.bob, &setup.episode(0)).await, None);
}

#[tokio::test]
async fn a_start_replacing_a_handoff_resumed_then_stopped_during_a_finished_retry_writes_no_finished()
 {
    let (proxy, setup) = setup_with_proxy().await;
    let (first, second) = (setup.episode(0), setup.episode(1));
    setup.ended_with_finished_failing(&proxy, 1).await;

    // The retry at the tick at 630 waits 2.5 seconds to learn whom the key acts for.
    // Meanwhile Bob resumes Kitchen from the App (the episode plays again from its start),
    // it is stopped from the Sonos app, and Bob starts his episode on it: Kitchen didn't play
    // to the end since it was resumed, so nothing is written for Alice.
    let asked = proxy.identity_requests();
    proxy.delay_next_identity(Duration::from_millis(2500));
    setup.advance(25).await;
    let retrying = async || proxy.identity_requests() > asked;
    eventually("the retry never asked", WITHIN, retrying).await;
    setup.resume().await;
    setup.household.stop(&setup.kitchen);
    setup.start(&setup.bobs, &second, None).await;
    assert_eq!(
        setup.progress(&setup.alice, &first).await,
        Some((590.0, false))
    );
    assert_bobs_handoff_listed(&setup).await;
}

#[tokio::test]
async fn a_group_resumed_paused_and_stopped_during_a_finished_retry_is_not_finished() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    setup.ended_with_finished_failing(&proxy, 1).await;

    // The retry at the tick at 630 waits 2.5 seconds to learn whom the key acts for.
    // Meanwhile Bob resumes Kitchen from the App (the episode plays again from its start) and
    // pauses it at once, writing 0, and it is stopped from the Sonos app: the retry then
    // finds Kitchen stopped, last seen paused, and writes nothing; the Handoff stays.
    let asked = proxy.identity_requests();
    proxy.delay_next_identity(Duration::from_millis(2500));
    setup.advance(25).await;
    let retrying = async || proxy.identity_requests() > asked;
    eventually("the retry never asked", WITHIN, retrying).await;
    setup.resume().await;
    setup.pause().await;
    setup.household.stop(&setup.kitchen);
    let reads = setup.asked(READ);
    let read = async || setup.asked(READ) > reads;
    eventually("the retry never read Kitchen", WITHIN, read).await;
    setup.settle().await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((0.0, false))
    );
    assert_eq!(setup.handoffs().await, 1);
}

#[tokio::test]
async fn a_group_that_cant_be_read_past_its_end_is_read_at_a_bounded_rate() {
    let setup = setup().await;
    let episode = setup.episode(0);
    setup.start(&setup.alices, &episode, None).await;
    setup.advance(590).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (590.0, false))
        .await;

    // Kitchen stops answering what it plays; the Bridge expected the end at 600 seconds, so
    // reads it every second from 595 until 630.
    setup
        .household
        .set_fault(&setup.kitchen, Fault::FailTransportInfo);
    let reads = setup.asked(READ);
    setup.advance(4).await;
    for _ in 595..=630 {
        setup.advance_and_read(1).await;
    }
    // Then only at the ticks: the next at 660.
    for _ in 631..660 {
        setup.advance(1).await;
    }
    setup.advance_and_settle(1).await;
    assert_eq!(setup.asked(READ), reads + 36 + 1);
}

#[tokio::test]
async fn a_key_rejected_on_a_cadence_write_is_revoked_ending_the_handoff() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    setup.start(&setup.alices, &episode, None).await;

    proxy.reject_progress_writes();
    setup.advance(30).await;
    setup.wait_until_no_handoff().await;
    assert_eq!(
        setup.bridge.api().current_user_error(&setup.alices).await,
        SESSION_INVALID
    );
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);

    // Nothing supervises Kitchen any more: no read, and no write once writes pass again.
    proxy.pass_progress_writes();
    setup.assert_kitchen_unsupervised().await;
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);
}

#[tokio::test]
async fn replaying_a_finished_item_clears_finished_once_progress_is_written() {
    let setup = setup().await;
    let episode = setup.episode(0);
    setup
        .set_progress(&setup.alice, &episode, 600.0, true)
        .await;

    // Finished, it starts at 0; the first Progress written leaves it unfinished.
    setup.start(&setup.alices, &episode, None).await;
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (30.0, false))
        .await;

    // Marked Finished again by her own player, a pause's Progress clears it too.
    setup
        .set_progress(&setup.alice, &episode, 600.0, true)
        .await;
    setup.advance(10).await;
    setup.pause().await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((40.0, false))
    );
}

#[tokio::test]
async fn replaying_a_finished_item_clears_finished_also_at_the_position_it_was_finished_at() {
    let setup = setup().await;
    let episode = setup.episode(0);
    // Marked Finished 30 seconds in: the first tick writes that same position.
    setup.set_progress(&setup.alice, &episode, 30.0, true).await;

    setup.start(&setup.alices, &episode, None).await;
    setup.advance_and_settle(30).await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((30.0, false))
    );
}

#[tokio::test]
async fn pausing_a_replayed_finished_item_at_its_start_clears_finished() {
    let setup = setup().await;
    let episode = setup.episode(0);
    // Marked Finished without ever being played.
    setup.set_progress(&setup.alice, &episode, 0.0, true).await;

    setup.start(&setup.alices, &episode, None).await;
    setup.pause().await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((0.0, false))
    );
}

#[tokio::test]
async fn replacing_a_handoff_writes_its_final_progress_before_the_new_one_starts() {
    let setup = setup().await;
    let (first, second) = (setup.episode(0), setup.episode(1));
    setup.start(&setup.alices, &first, None).await;
    setup.advance(100).await;
    setup
        .wait_for_progress(&setup.alice, &first, (100.0, false))
        .await;
    setup.advance(10).await;

    // Bob starts another episode on Kitchen: Alice's Handoff is replaced at 110 seconds, the
    // Progress written for her, with her key; none for Bob.
    setup.start(&setup.bobs, &second, None).await;
    assert_eq!(
        setup.progress(&setup.alice, &first).await,
        Some((110.0, false))
    );
    assert_eq!(setup.progress(&setup.bob, &first).await, None);

    // From here on Bob's Handoff gets Progress at its ticks, Alice's none.
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.bob, &second, (30.0, false))
        .await;
    setup.settle().await;
    assert_eq!(
        setup.progress(&setup.alice, &first).await,
        Some((110.0, false))
    );
    assert_eq!(setup.progress(&setup.alice, &second).await, None);

    // Alice replaces Bob's on a Kitchen that refuses to play it: Bob's Progress was written
    // before Kitchen was told to, so it is there although the start failed.
    setup.advance(5).await;
    setup
        .household
        .set_fault(&setup.kitchen, Fault::RefusePlayback);
    let refused = setup
        .bridge
        .api()
        .start_handoff_error(&setup.alices, &first, setup.kitchen.uuid())
        .await;
    assert_eq!(refused, GROUP_FAILED);
    assert_eq!(
        setup.progress(&setup.bob, &second).await,
        Some((35.0, false))
    );

    // Neither Handoff is left, and nothing writes Bob's Progress any more.
    assert_eq!(setup.handoffs().await, 0);
    setup.assert_kitchen_unsupervised().await;
    assert_eq!(
        setup.progress(&setup.bob, &second).await,
        Some((35.0, false))
    );
}

/// Alice's Handoff of episode 0 (600 seconds) on Kitchen, started at `from` seconds, and
/// Kitchen read by the Bridge once time moved on by `played` seconds; time stands still from
/// there, so Kitchen isn't read again.
async fn alice_read_at(setup: &Setup, from: f64, played: u64) {
    let first = setup.episode(0);
    setup.set_progress(&setup.alice, &first, from, false).await;
    let reads = setup.asked(READ);
    setup
        .start(&setup.alices, &first, Some(EPISODE_DURATIONS[0]))
        .await;
    if played == 0 {
        let read = async || setup.asked(READ) > reads;
        eventually("the Bridge never read Kitchen", WITHIN, read).await;
    } else {
        setup.advance_and_read(played).await;
    }
    setup.settle().await;
}

/// Alice's Handoff of episode 0 (600 seconds) on Kitchen, read by the Bridge a second before
/// the end, then paused by Bob from the App (writing 599 seconds) and stopped from the Sonos
/// app before the Bridge reads it again; time stands still from there.
async fn alice_paused_then_stopped_a_second_before_the_end(setup: &Setup) {
    alice_read_at(setup, 599.0, 0).await;
    setup.pause().await;
    setup.household.stop(&setup.kitchen);
}

/// Asserts Bob's Handoff of episode 1 is the one listed.
async fn assert_bobs_handoff_listed(setup: &Setup) {
    let listed = setup.bridge.api().handoffs(&setup.bobs).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (&listed[0].item, &listed[0].started_by.user_id),
        (&setup.episode(1), &setup.bob.id)
    );
}

#[tokio::test]
async fn a_start_replacing_a_handoff_stopped_a_second_before_the_end_writes_finished() {
    let setup = setup().await;
    let (first, length) = (setup.episode(0), EPISODE_DURATIONS[0]);
    // A second before the end, the Bridge reads Kitchen at once.
    alice_read_at(&setup, 599.0, 0).await;

    // Stopped from the Sonos app before the Bridge reads it again (telling 0, as at the end
    // of a URL), and Bob starts his episode on it: within a second of the end, it can't be
    // told from one played to its end (Sonos tells whole seconds), so it counts as that.
    setup.household.stop(&setup.kitchen);
    setup.start(&setup.bobs, &setup.episode(1), None).await;
    assert_eq!(
        setup.progress(&setup.alice, &first).await,
        Some((length, true))
    );
    assert_bobs_handoff_listed(&setup).await;
}

#[tokio::test]
async fn a_start_replacing_a_handoff_played_to_its_end_before_the_next_read_writes_finished() {
    let setup = setup().await;
    let (first, length) = (setup.episode(0), EPISODE_DURATIONS[0]);
    // Read every second from 595 to 598 seconds, playing.
    alice_read_at(&setup, 590.0, 5).await;
    setup.read_every_second(3).await;

    // Kitchen can't tell what it plays for the reads at 599 and 600, and stops at its end at
    // 600, 2 seconds after it was last read playing. The Bridge's next read waits for time
    // to move on, so Bob's start, once Kitchen tells again, is the first to find it stopped.
    setup
        .household
        .set_fault(&setup.kitchen, Fault::FailTransportInfo);
    setup.read_every_second(2).await;
    assert_eq!(setup.household.transport_state(&setup.kitchen), "STOPPED");
    setup.household.set_fault(&setup.kitchen, Fault::None);
    setup.start(&setup.bobs, &setup.episode(1), None).await;
    assert_eq!(
        setup.progress(&setup.alice, &first).await,
        Some((length, true))
    );
    assert_bobs_handoff_listed(&setup).await;
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");
}

#[tokio::test]
async fn a_start_replacing_a_handoff_stopped_short_of_the_end_writes_no_finished() {
    let setup = setup().await;
    let first = setup.episode(0);
    // Read 5 seconds before the end, then stopped from the Sonos app.
    alice_read_at(&setup, 590.0, 5).await;

    setup.household.stop(&setup.kitchen);
    setup.start(&setup.bobs, &setup.episode(1), None).await;
    assert_eq!(
        setup.progress(&setup.alice, &first).await,
        Some((590.0, false))
    );
    assert_bobs_handoff_listed(&setup).await;
}

#[tokio::test]
async fn takeover_source_writes_only_old_stream_progress_and_revokes_its_url() {
    let setup = setup().await;
    let episode = setup.episode(0);
    setup
        .start(&setup.alices, &episode, Some(EPISODE_DURATIONS[0]))
        .await;
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (30.0, false))
        .await;
    setup.pause().await;
    let stream = setup.household.offered(&setup.kitchen).pop().unwrap();
    let other_item = setup.episode(1);
    setup
        .bridge
        .api()
        .start_handoff(&setup.bobs, &other_item, setup.office.uuid())
        .await;
    setup.household.fetched(&setup.office).await;
    setup.advance(7).await;
    let other = FakeTrack::new("External radio", Duration::from_secs(3600));
    setup.household.play(&setup.kitchen, other.clone());
    eventually("Kitchen Handoff did not end", WITHIN, async || {
        setup
            .bridge
            .api()
            .handoffs(&setup.bobs)
            .await
            .iter()
            .all(|handoff| handoff.group != setup.kitchen.uuid())
    })
    .await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((30.0, false))
    );
    assert_eq!(setup.progress(&setup.bob, &episode).await, None);
    assert_eq!(setup.progress(&setup.alice, &other_item).await, None);
    assert_eq!(setup.handoffs().await, 1);
    assert_eq!(
        reqwest::get(&stream).await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND
    );
    let upnp = sonos::Upnp::start(setup.household.ssdp(), Arc::new(clock::SystemClock));
    assert_eq!(
        upnp.playback(setup.kitchen.uuid())
            .await
            .unwrap()
            .uri
            .as_deref(),
        Some(other.uri.as_str())
    );
    let group = setup
        .bridge
        .api()
        .groups(&setup.bobs)
        .await
        .into_iter()
        .find(|group| group.id == setup.kitchen.uuid())
        .unwrap();
    assert!(group.handoff.is_none());
}

#[tokio::test]
async fn takeover_regrouping_ends_both_affected_groups_but_not_an_unrelated_group() {
    let setup = setup().await;
    let third = setup.household.add_player("Bedroom");
    setup.start(&setup.alices, &setup.episode(0), None).await;
    setup
        .bridge
        .api()
        .start_handoff(&setup.bobs, &setup.episode(1), setup.office.uuid())
        .await;
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &setup.episode(0), (30.0, false))
        .await;
    setup
        .wait_for_progress(&setup.bob, &setup.episode(1), (30.0, false))
        .await;
    setup.household.join(&third, &setup.kitchen);
    eventually(
        "affected Handoffs stayed active after regrouping",
        WITHIN,
        async || {
            let handoffs = setup.bridge.api().handoffs(&setup.bobs).await;
            handoffs.len() == 1 && handoffs[0].group == setup.office.uuid()
        },
    )
    .await;
    assert_eq!(
        setup.progress(&setup.alice, &setup.episode(0)).await,
        Some((30.0, false))
    );
    assert_eq!(
        setup.progress(&setup.bob, &setup.episode(1)).await,
        Some((30.0, false))
    );
}

#[tokio::test]
async fn takeover_coordinator_disappearance_ends_both_affected_handoffs() {
    let setup = setup().await;
    setup.start(&setup.alices, &setup.episode(0), None).await;
    setup
        .bridge
        .api()
        .start_handoff(&setup.bobs, &setup.episode(1), setup.office.uuid())
        .await;
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &setup.episode(0), (30.0, false))
        .await;
    setup
        .wait_for_progress(&setup.bob, &setup.episode(1), (30.0, false))
        .await;
    setup.household.join(&setup.kitchen, &setup.office);
    setup.wait_until_no_handoff().await;
    assert_eq!(
        setup.progress(&setup.alice, &setup.episode(0)).await,
        Some((30.0, false))
    );
    assert_eq!(
        setup.progress(&setup.bob, &setup.episode(1)).await,
        Some((30.0, false))
    );
}

#[tokio::test]
async fn takeover_after_renewal_ignores_bridge_controls_replacement_and_snapshots() {
    let setup = setup().await;
    setup.start(&setup.alices, &setup.episode(0), None).await;
    setup.pause().await;
    setup.resume().await;
    setup
        .bridge
        .api()
        .skip_group(&setup.bobs, setup.kitchen.uuid(), Skip::Forward30)
        .await;
    setup.settle().await;
    setup.start(&setup.bobs, &setup.episode(1), None).await;
    setup.household.notify();
    setup.advance(31).await;
    eventually("GENA subscriptions did not renew", WITHIN, async || {
        setup.household.renewals() >= 4
    })
    .await;
    setup
        .wait_for_progress(&setup.bob, &setup.episode(1), (31.0, false))
        .await;
    assert_eq!(setup.handoffs().await, 1);
    setup.household.play(
        &setup.kitchen,
        FakeTrack::new("External", Duration::from_secs(3600)),
    );
    setup.wait_until_no_handoff().await;
    assert_eq!(
        setup.progress(&setup.bob, &setup.episode(1)).await,
        Some((31.0, false))
    );
}

#[tokio::test]
async fn takeover_subscription_failure_recovers_and_never_counts_as_a_source_change() {
    let setup = setup().await;
    setup.start(&setup.alices, &setup.episode(0), None).await;
    setup.household.reject_subscriptions(true);
    setup.advance(31).await;
    setup
        .wait_for_progress(&setup.alice, &setup.episode(0), (31.0, false))
        .await;
    assert_eq!(setup.handoffs().await, 1);
    setup.household.reject_subscriptions(false);
    setup.advance(6).await;
    setup.household.play(
        &setup.kitchen,
        FakeTrack::new("External", Duration::from_secs(3600)),
    );
    setup.advance(6).await;
    setup.wait_until_no_handoff().await;
    assert_eq!(
        setup.progress(&setup.alice, &setup.episode(0)).await,
        Some((31.0, false))
    );
}

#[tokio::test]
async fn takeover_during_a_bridge_pause_is_not_suppressed() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    setup.start(&setup.alices, &episode, None).await;
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (30.0, false))
        .await;
    let before = proxy.progress_writes();
    let held = proxy.hold_next_progress_write();
    let pause = setup.pause();
    let external = async {
        eventually("pause write never arrived", WITHIN, async || {
            proxy.progress_writes() > before
        })
        .await;
        setup.household.play(
            &setup.kitchen,
            FakeTrack::new("External", Duration::from_secs(3600)),
        );
        held.release().await;
    };
    tokio::join!(pause, external);
    setup.wait_until_no_handoff().await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((30.0, false))
    );
}

#[tokio::test]
async fn takeover_writes_progress_before_ending_and_ends_even_if_the_write_fails() {
    for fail in [false, true] {
        let (proxy, setup) = setup_with_proxy().await;
        let episode = setup.episode(0);
        setup.start(&setup.alices, &episode, None).await;
        setup.advance(30).await;
        setup
            .wait_for_progress(&setup.alice, &episode, (30.0, false))
            .await;
        if fail {
            proxy.fail_progress_writes(1);
        }
        let before = proxy.progress_writes();
        let held = (!fail).then(|| proxy.hold_next_progress_write());
        setup.household.play(
            &setup.kitchen,
            FakeTrack::new("External", Duration::from_secs(3600)),
        );
        eventually("final Progress write never arrived", WITHIN, async || {
            proxy.progress_writes() > before
        })
        .await;
        if let Some(held) = held {
            assert_eq!(
                setup.handoffs().await,
                1,
                "Handoff ended before its final write completed"
            );
            held.release().await;
        }
        setup.wait_until_no_handoff().await;
        assert_eq!(
            setup.progress(&setup.alice, &episode).await,
            Some((30.0, false))
        );
        assert_eq!(proxy.progress_writes_failed(), usize::from(fail));
    }
}

#[tokio::test]
async fn takeover_failed_state_read_retries_without_ending_on_the_failure() {
    let setup = setup().await;
    let episode = setup.episode(0);
    setup.start(&setup.alices, &episode, None).await;
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (30.0, false))
        .await;
    setup
        .household
        .set_fault(&setup.kitchen, Fault::FailTransportInfo);
    let reads = setup.asked(READ);
    setup.household.play(
        &setup.kitchen,
        FakeTrack::new("External", Duration::from_secs(3600)),
    );
    eventually("event was never reconciled", WITHIN, async || {
        setup.asked(READ) > reads
    })
    .await;
    setup.settle().await;
    assert_eq!(setup.handoffs().await, 1);
    setup.household.set_fault(&setup.kitchen, Fault::None);
    setup.advance(1).await;
    setup.wait_until_no_handoff().await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((30.0, false))
    );
}

#[tokio::test]
async fn takeover_final_progress_never_follows_a_key_moved_to_another_user() {
    let setup = setup().await;
    let episode = setup.episode(0);
    let alice_reader = AbsUser {
        id: setup.alice.id.clone(),
        username: setup.alice.username.clone(),
        api_key: setup.abs.new_api_key(&setup.alice.id).await,
    };
    setup.set_progress(&setup.bob, &episode, 77.0, false).await;
    setup.start(&setup.alices, &episode, None).await;
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (30.0, false))
        .await;
    setup
        .abs
        .move_api_key(&setup.alice.api_key, &setup.bob.id)
        .await;
    setup.household.play(
        &setup.kitchen,
        FakeTrack::new("External", Duration::from_secs(3600)),
    );
    setup.wait_until_no_handoff().await;
    assert_eq!(
        setup.progress(&alice_reader, &episode).await,
        Some((30.0, false))
    );
    assert_eq!(
        setup.progress(&setup.bob, &episode).await,
        Some((77.0, false))
    );
    assert_eq!(
        setup.bridge.api().home_error(&setup.alices).await,
        SESSION_INVALID
    );
}

#[tokio::test]
async fn takeover_of_a_stopped_stream_retains_its_last_reliable_position() {
    let setup = setup().await;
    let episode = setup.episode(0);
    setup.start(&setup.alices, &episode, None).await;
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (30.0, false))
        .await;
    setup.household.stop(&setup.kitchen);
    setup.advance_and_settle(30).await;
    assert_eq!(setup.handoffs().await, 1);
    setup.household.join(&setup.office, &setup.kitchen);
    setup.wait_until_no_handoff().await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((30.0, false))
    );
}

/// Asserts that Bob's start, which replaced Alice's Handoff at 110 seconds after taking
/// `took`, went on although her Progress write didn't land: within the start's time, his
/// Handoff listed, her Progress as it was at 100 seconds, and nothing written with either
/// key in the other's Item.
async fn assert_replaced_without_progress(setup: &Setup, took: Duration) {
    let (first, second) = (setup.episode(0), setup.episode(1));
    assert!(took < START_TIME, "the start took {took:?}");
    assert_bobs_handoff_listed(setup).await;
    assert_eq!(
        setup.progress(&setup.alice, &first).await,
        Some((100.0, false))
    );
    assert_eq!(setup.progress(&setup.bob, &first).await, None);
    assert_eq!(setup.progress(&setup.alice, &second).await, None);
}

/// Alice's Handoff on Kitchen at 110 seconds, her Progress written at 100.
async fn alice_at_110(setup: &Setup) {
    let first = setup.episode(0);
    setup.start(&setup.alices, &first, None).await;
    setup.advance(100).await;
    setup
        .wait_for_progress(&setup.alice, &first, (100.0, false))
        .await;
    setup.advance(10).await;
}

#[tokio::test]
async fn a_rejected_replaced_progress_write_does_not_hold_up_the_new_start() {
    let (proxy, setup) = setup_with_proxy().await;
    alice_at_110(&setup).await;

    proxy.reject_progress_writes();
    let started = Instant::now();
    setup.start(&setup.bobs, &setup.episode(1), None).await;
    assert_replaced_without_progress(&setup, started.elapsed()).await;
    // Alice's key was revoked, and her session with it.
    assert_eq!(
        setup.bridge.api().current_user_error(&setup.alices).await,
        SESSION_INVALID
    );
}

#[tokio::test]
async fn a_late_replaced_progress_write_does_not_hold_up_the_new_start() {
    let (proxy, setup) = setup_with_proxy().await;
    alice_at_110(&setup).await;

    // The Library Server takes too long to write the replaced Handoff's Progress.
    proxy.delay_progress_writes(Duration::from_secs(20));
    let started = Instant::now();
    setup.start(&setup.bobs, &setup.episode(1), None).await;
    assert_replaced_without_progress(&setup, started.elapsed()).await;
}

#[tokio::test]
async fn a_late_group_read_for_the_replaced_progress_does_not_hold_up_the_new_start() {
    let setup = setup().await;
    alice_at_110(&setup).await;

    // Kitchen tells where it is too late for the replaced Handoff's write.
    let slow = Fault::SlowPosition(Duration::from_secs(20));
    setup.household.set_fault(&setup.kitchen, slow);
    let started = Instant::now();
    setup.start(&setup.bobs, &setup.episode(1), None).await;
    assert_replaced_without_progress(&setup, started.elapsed()).await;
}
