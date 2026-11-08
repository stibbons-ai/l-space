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

/// The SOAP action a Bridge reads a Group's transport state with, as `sonos-fake` records it.
const READ: &str = "AVTransport#GetTransportInfo";

/// How long a test waits for the Bridge to act on what it observes.
const WITHIN: Duration = Duration::from_secs(10);

/// A household of Kitchen and Office, a Bridge searching it and following its clock, with the
/// shared Audiobookshelf as its Library Server, and two users logged in: Alice, who starts
/// Handoffs, and Bob, who controls them or starts his own.
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
    let (household, clock) = Household::with_manual_clock();
    let kitchen = household.add_player("Kitchen");
    household.add_player("Office");
    let config = format!(
        "{}{}",
        home_with_sonos_toml(&url, household.ssdp()),
        clock_toml(household.clock_address())
    );
    let bridge = TestBridge::start(&config);
    let (alice, bob) = (abs.new_user().await, abs.new_user().await);
    let alices = bridge.api().login(&url, &alice.api_key.key).await;
    let bobs = bridge.api().login(&url, &bob.api_key.key).await;
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

    /// `user`'s Progress in `item`, read from Audiobookshelf with their own key:
    /// `(currentTime, isFinished)`, if any.
    async fn progress(&self, user: &AbsUser, item: &ItemId) -> Option<(f64, bool)> {
        let episode = item.episode.as_deref();
        self.abs.progress(user, &item.library_item, episode).await
    }

    /// Waits until `user`'s Progress in `item` is `expected`, at most [`WITHIN`].
    async fn wait_for_progress(&self, user: &AbsUser, item: &ItemId, expected: (f64, bool)) {
        let deadline = Instant::now() + WITHIN;
        loop {
            let progress = self.progress(user, item).await;
            if progress == Some(expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Progress {progress:?}, not {expected:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// How often Kitchen was sent SOAP action `action`.
    fn asked(&self, action: &str) -> usize {
        let actions = self.household.actions_of(&self.kitchen);
        actions.iter().filter(|sent| *sent == action).count()
    }

    /// Moves time on by `seconds` and waits until the Bridge read Kitchen afterwards and was
    /// done with what it read: a pause by Bob (on a Group already paused, which changes and
    /// writes nothing) takes the Group's turn after the Bridge's read.
    async fn advance_and_settle(&self, seconds: u64) {
        let before = self.asked(READ);
        self.advance(seconds).await;
        let deadline = Instant::now() + WITHIN;
        while self.asked(READ) == before {
            assert!(Instant::now() < deadline, "the Bridge never read Kitchen");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let paused = self
            .bridge
            .api()
            .pause_group(&self.bobs, self.kitchen.uuid())
            .await;
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
    // Only the starter's.
    assert_eq!(setup.progress(&setup.bob, &episode).await, None);

    // Paused, nothing is written: Alice's own player moves her Progress meanwhile, and it
    // stays as that left it.
    setup
        .bridge
        .api()
        .pause_group(&setup.bobs, setup.kitchen.uuid())
        .await;
    let (podcast, first) = (&episode.library_item, episode.episode.as_deref());
    setup
        .abs
        .set_progress(&setup.alice, podcast, first, 42.0, false)
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

    // Kitchen plays to the end of the episode and stops there.
    setup.advance(15).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (length, true))
        .await;
    let api = setup.bridge.api();
    let deadline = Instant::now() + WITHIN;
    while !api.handoffs(&setup.bobs).await.is_empty() {
        assert!(Instant::now() < deadline, "the Handoff never ended");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let groups = api.groups(&setup.bobs).await;
    let kitchen = groups.iter().find(|group| group.name == "Kitchen").unwrap();
    assert_eq!(
        (kitchen.state, &kitchen.handoff),
        (Some(TransportState::Stopped), &None)
    );
    // Nothing else was started on it.
    assert_eq!(setup.asked("AVTransport#SetAVTransportURI"), 1);
    assert_eq!(setup.asked("AVTransport#Play"), 1);
    setup.advance(60).await;
    assert_eq!(setup.household.transport_state(&setup.kitchen), "STOPPED");
    assert_eq!(
        setup.progress(&setup.alice, &episode).await,
        Some((length, true))
    );
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
    setup
        .bridge
        .api()
        .pause_group(&setup.bobs, setup.kitchen.uuid())
        .await;
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
async fn replaying_a_finished_item_clears_finished_once_progress_is_written() {
    let setup = setup().await;
    let episode = setup.episode(0);
    let (podcast, first) = (&episode.library_item, episode.episode.as_deref());
    setup
        .abs
        .set_progress(&setup.alice, podcast, first, 600.0, true)
        .await;

    // Finished, it starts at 0; the first Progress written leaves it unfinished.
    setup.start(&setup.alices, &episode, None).await;
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.alice, &episode, (30.0, false))
        .await;

    // Marked Finished again by her own player, a pause's Progress clears it too.
    setup
        .abs
        .set_progress(&setup.alice, podcast, first, 600.0, true)
        .await;
    setup.advance(10).await;
    setup
        .bridge
        .api()
        .pause_group(&setup.bobs, setup.kitchen.uuid())
        .await;
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

    // Only Bob's Handoff is supervised from here on.
    setup.advance(30).await;
    setup
        .wait_for_progress(&setup.bob, &second, (30.0, false))
        .await;
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
