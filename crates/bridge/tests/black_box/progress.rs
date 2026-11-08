//! Handoff supervision between App API calls: while a Group plays a Handoff, its Progress is
//! written for the starter about every 30 seconds; played to the end of its Item, it is
//! written Finished there and the Handoff ends; a start replacing a Handoff writes the
//! replaced one's Progress first. Against the seeded Audiobookshelf (read back with each
//! user's own key) and `sonos-fake` players, with the Bridge following the household's manual
//! clock through its test-only clock hook, so time moves only when a test advances it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{ItemId, Session, TransportState};
use sonos_fake::{Fault, Household, PlayerId};

use test_support::audiobookshelf::{AbsUser, Audiobookshelf, EPISODE_DURATIONS};
use test_support::bridge::{GROUP_FAILED, TestBridge, clock_toml, home_with_sonos_toml};
use test_support::{WITHIN, eventually};

use crate::library_proxy::LibraryProxy;

/// The SOAP action a Bridge reads a Group's transport state with, as `sonos-fake` records it.
const READ: &str = "AVTransport#GetTransportInfo";

/// The SOAP action a Bridge reads a Group's position with, alongside [`READ`].
const READ_POSITION: &str = "AVTransport#GetPositionInfo";

/// How long, in real time, a test watches for something that must not happen.
const QUIET: Duration = Duration::from_millis(300);

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
    household.add_player("Office");
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
        let seeded = self.abs.seeded();
        ItemId {
            library_item: seeded.podcast.clone(),
            episode: Some(seeded.episodes[n].clone()),
        }
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

    /// Asserts the Bridge reads Kitchen no more than `reads` times in all, watching a while.
    async fn assert_reads(&self, reads: usize) {
        tokio::time::sleep(QUIET).await;
        assert_eq!(self.asked(READ), reads);
    }

    /// `user`'s Progress in `item`, read from Audiobookshelf with their own key:
    /// `(currentTime, isFinished)`, if any.
    async fn progress(&self, user: &AbsUser, item: &ItemId) -> Option<(f64, bool)> {
        let episode = item.episode.as_deref();
        self.abs.progress(user, &item.library_item, episode).await
    }

    /// Records `user`'s Progress in `item` in Audiobookshelf, as their own player would.
    async fn set_progress(&self, user: &AbsUser, item: &ItemId, current_time: f64, done: bool) {
        let episode = item.episode.as_deref();
        self.abs
            .set_progress(user, &item.library_item, episode, current_time, done)
            .await;
    }

    /// Waits until `user`'s Progress in `item` is `expected`.
    async fn wait_for_progress(&self, user: &AbsUser, item: &ItemId, expected: (f64, bool)) {
        let what = format!("Progress never became {expected:?}");
        eventually(&what, WITHIN, async || {
            self.progress(user, item).await == Some(expected)
        })
        .await;
    }

    /// Waits until no Handoff is listed.
    async fn wait_until_no_handoff(&self) {
        let api = self.bridge.api();
        let ended = async || api.handoffs(&self.bobs).await.is_empty();
        eventually("the Handoff never ended", WITHIN, ended).await;
    }

    /// How often Kitchen was sent SOAP action `action`.
    fn asked(&self, action: &str) -> usize {
        let actions = self.household.actions_of(&self.kitchen);
        actions.iter().filter(|sent| *sent == action).count()
    }

    /// Bob pauses Kitchen.
    async fn pause(&self) {
        let kitchen = self.kitchen.uuid();
        let paused = self.bridge.api().pause_group(&self.bobs, kitchen).await;
        assert_eq!(paused.state, TransportState::Paused);
    }
}

#[tokio::test]
async fn a_playing_handoff_writes_the_starters_progress_every_30_seconds() {
    let setup = setup().await;
    let episode = setup.episode(0);
    setup.start(&setup.alices, &episode, None).await;

    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (30.0, false))
        .await;
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
    let reads = setup.asked(READ);
    setup.advance(60).await;
    setup.assert_reads(reads).await;
    assert_eq!(setup.household.transport_state(&setup.kitchen), "STOPPED");
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((length, true))
    );
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
    assert_eq!(setup.bridge.api().handoffs(&setup.bobs).await.len(), 1);
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
    assert_eq!(setup.bridge.api().handoffs(&setup.bobs).await.len(), 1);
}

#[tokio::test]
async fn a_finished_write_that_fails_is_tried_again() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    let length = EPISODE_DURATIONS[0];
    setup.start(&setup.alices, &episode, Some(length)).await;
    setup.advance(590).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (590.0, false))
        .await;
    setup.advance_and_read(5).await;

    // Kitchen stops at the end, and the Library Server fails the Finished write: the Handoff
    // stays for another try.
    proxy.fail_progress_writes(1);
    setup.advance(10).await;
    let failed = async || proxy.progress_writes_failed() == 1;
    eventually("the Finished write never came", WITHIN, failed).await;
    setup.settle().await;
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((590.0, false))
    );
    assert_eq!(setup.bridge.api().handoffs(&setup.bobs).await.len(), 1);

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
    setup
        .start(&setup.alices, &episode, Some(EPISODE_DURATIONS[0]))
        .await;
    setup.advance(590).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (590.0, false))
        .await;
    setup.advance_and_read(5).await;

    proxy.fail_progress_writes(4);
    setup.advance(10).await;
    for tries in 1..=2 {
        let failed = async || proxy.progress_writes_failed() == tries;
        eventually("a Finished write never came", WITHIN, failed).await;
        setup.settle().await;
        assert_eq!(setup.bridge.api().handoffs(&setup.bobs).await.len(), 1);
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
async fn a_group_that_cant_be_read_past_its_end_is_read_at_a_bounded_rate() {
    let setup = setup().await;
    let episode = setup.episode(0);
    setup.start(&setup.alices, &episode, None).await;
    setup.advance(590).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (590.0, false))
        .await;

    // Kitchen stops answering what it plays; the Bridge expected the end at 600 seconds.
    setup
        .household
        .set_fault(&setup.kitchen, Fault::FailTransportInfo);
    let reads = setup.asked(READ);
    setup.advance_and_settle(20).await;
    setup.assert_reads(reads + 1).await;
    setup.advance_and_settle(60).await;
    setup.assert_reads(reads + 2).await;
}

#[tokio::test]
async fn a_key_rejected_on_a_cadence_write_is_revoked_ending_the_handoff() {
    let (proxy, setup) = setup_with_proxy().await;
    let episode = setup.episode(0);
    setup.start(&setup.alices, &episode, None).await;

    proxy.reject_progress_writes();
    setup.advance(30).await;
    setup.wait_until_no_handoff().await;
    assert_eq!(setup.progress(&setup.alice, &episode).await, None);

    // Nothing supervises Kitchen any more: no read, no write once writes pass again.
    proxy.pass_progress_writes();
    let reads = setup.asked(READ);
    setup.advance(30).await;
    setup.assert_reads(reads).await;
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

    // Only Bob's Handoff is supervised from here on: Kitchen is read once at the next tick.
    let reads = setup.asked(READ);
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.bob, &second, (30.0, false))
        .await;
    setup.assert_reads(reads + 1).await;
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
}

#[tokio::test]
async fn a_late_replaced_progress_write_does_not_hold_up_the_new_start() {
    let setup = setup().await;
    alice_at_110(&setup).await;

    // Kitchen tells where it is too late for the replaced Handoff's write.
    let slow = Fault::SlowPosition(Duration::from_secs(20));
    setup.household.set_fault(&setup.kitchen, slow);
    let started = Instant::now();
    setup.start(&setup.bobs, &setup.episode(1), None).await;
    assert_replaced_without_progress(&setup, started.elapsed()).await;
}
