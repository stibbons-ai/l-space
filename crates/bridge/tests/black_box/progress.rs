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
use sonos_fake::{Fault, Household, PlayerId};

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

    /// Asserts that nothing supervises Kitchen any more: Bob starts a witness Handoff on
    /// Office, and time moves on to its first tick. Once the witness's Progress is written
    /// there and Kitchen's turn is free, a supervisor of Kitchen woken by the same tick would
    /// have read Kitchen.
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
    /// stopped at 600 seconds, and its Finished write failed (as the next `failures` will);
    /// 605 seconds in.
    async fn ended_with_finished_failing(&self, proxy: &LibraryProxy, failures: usize) {
        let episode = self.episode(0);
        self.start(&self.alices, &episode, Some(EPISODE_DURATIONS[0]))
            .await;
        self.advance(590).await;
        self.wait_for_progress(&self.alice, &episode, (590.0, false))
            .await;
        self.advance_and_read(5).await;
        proxy.fail_progress_writes(failures);
        self.advance(10).await;
        let failed = async || proxy.progress_writes_failed() == 1;
        eventually("the Finished write never came", WITHIN, failed).await;
        self.settle().await;
    }
}

#[tokio::test]
async fn a_playing_handoff_writes_the_starters_progress_every_30_seconds() {
    let setup = setup().await;
    let episode = setup.episode(0);
    setup.start(&setup.alices, &episode, None).await;
    let reads = setup.asked(READ);

    // Not before 30 seconds: then once, with a single read of Kitchen.
    setup.advance(29).await;
    setup.settle().await;
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);
    setup.advance(1).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (30.0, false))
        .await;
    setup.settle().await;
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
    setup.advance(10).await;
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
    setup.advance(10).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (length, true))
        .await;
    setup.wait_until_no_handoff().await;
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

    // Played again from outside the App, at its start: the Bridge finds it so at the next
    // tick, writes that, not Finished, and the Handoff goes on.
    setup
        .household
        .set_transport_state(&setup.kitchen, "PLAYING");
    setup.advance_and_settle(25).await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((0.0, false))
    );
    assert_eq!(setup.handoffs().await, 1);
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

/// Asserts that Bob's start, which replaced Alice's Handoff at 110 seconds after taking
/// `took`, went on although her Progress write didn't land: within the start's time, his
/// Handoff listed, her Progress as it was at 100 seconds, and nothing written with either
/// key in the other's Item.
async fn assert_replaced_without_progress(setup: &Setup, took: Duration) {
    let (first, second) = (setup.episode(0), setup.episode(1));
    assert!(took < START_TIME, "the start took {took:?}");
    let listed = setup.bridge.api().handoffs(&setup.bobs).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (&listed[0].item, &listed[0].started_by.user_id),
        (&second, &setup.bob.id)
    );
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
