//! Audiobooks in several audio files (#19): a Handoff plays all of an Item's files as its
//! Group's queue, Progress is the position in the whole Item, skips and seeks cross file
//! boundaries, previous and next chapter move between chapters, and the Item is Finished when
//! the Group stops at the end of its last file. Against the seeded "The Long Book" (files of
//! 40, 30 and 2 minutes, a chapter each) in the shared Audiobookshelf, read back with each
//! user's own key, and `sonos-fake` players playing queues, with the Bridge following the
//! household's manual clock through its test-only clock hook.

use std::sync::Arc;
use std::time::Duration;

use api::{ApiError, ItemId, Playback, Session, Skip, TransportState};
use reqwest::StatusCode;
use sonos_fake::{Household, PlayerId, Seek};

use test_support::audiobookshelf::{AbsUser, Audiobookshelf};
use test_support::bridge::{
    GroupControl, LIBRARY_SERVER_UNREACHABLE, TestBridge, clock_toml, home_with_sonos_toml,
};
use test_support::{WITHIN, eventually};

use crate::forwarder::Forwarder;
use crate::library_proxy::LibraryProxy;

/// The SOAP action a Bridge reads a Group's transport state with, as `sonos-fake` records it.
const READ: &str = "AVTransport#GetTransportInfo";

/// The SOAP action a Bridge reads a Group's position with, alongside [`READ`].
const READ_POSITION: &str = "AVTransport#GetPositionInfo";

/// "The Long Book"'s files' lengths in seconds, in playing order.
const FILES: [u64; 3] = [2400, 1800, 120];

/// Its length in seconds: all its files'.
const LENGTH: f64 = 4320.0;

/// The App API's answer to starting an Item a Group can't play.
const NOT_PLAYABLE: (StatusCode, ApiError) =
    (StatusCode::UNPROCESSABLE_ENTITY, ApiError::ItemNotPlayable);

/// A household of Kitchen and Office, whose queues' tracks are as long as "The Long Book"'s
/// files, a Bridge searching it and following its clock, with the shared Audiobookshelf as its
/// Library Server behind a forwarder, and two users logged in: Alice, who starts Handoffs, and
/// Bob, who controls them.
struct Setup {
    abs: Arc<Audiobookshelf>,
    forwarder: Forwarder,
    household: Household,
    clock: clock::ManualClock,
    kitchen: PlayerId,
    office: PlayerId,
    bridge: TestBridge,
    alice: AbsUser,
    alices: Session,
    bobs: Session,
}

async fn setup() -> Setup {
    let abs = Audiobookshelf::shared();
    let forwarder = Forwarder::to(abs.address()).await;
    let (household, clock) = Household::with_manual_clock();
    let kitchen = household.add_player("Kitchen");
    let office = household.add_player("Office");
    let lengths = FILES.map(Duration::from_secs);
    household.set_queue_lengths(&kitchen, &lengths);
    let url = forwarder.url();
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
        forwarder,
        household,
        clock,
        kitchen,
        office,
        bridge,
        alice,
        alices,
        bobs,
    }
}

impl Setup {
    /// "The Long Book".
    fn book(&self) -> ItemId {
        ItemId {
            library_item: self.abs.seeded().long_book.clone(),
            episode: None,
        }
    }

    /// Alice starts the book on Kitchen, with her Progress in it at `progress` seconds first,
    /// if given; waits until Kitchen fetched the file it starts in.
    async fn start(&self, progress: Option<f64>) {
        let book = self.book();
        if let Some(current_time) = progress {
            let abs = &self.abs;
            abs.set_item_progress(&self.alice, &book, current_time, false)
                .await;
        }
        let kitchen = self.kitchen.uuid();
        self.bridge
            .api()
            .start_handoff(&self.alices, &book, kitchen)
            .await;
        self.household.fetched(&self.kitchen).await;
    }

    /// Which track of its queue Kitchen plays (1-based), and how far into it it is.
    fn track(&self) -> (Option<usize>, Duration) {
        let number = self.household.track_number(&self.kitchen);
        (number, self.household.position(&self.kitchen))
    }

    /// Moves time on by `seconds`, and waits until the Bridge has seen it.
    async fn advance(&self, seconds: u64) {
        self.clock.advance(Duration::from_secs(seconds));
        self.household.clock_followed().await;
    }

    /// Moves time on by `seconds`, and waits until the Bridge read Kitchen afterwards.
    async fn advance_and_read(&self, seconds: u64) {
        let asked = |action| self.household.asked(&self.kitchen, action);
        let before = (asked(READ), asked(READ_POSITION));
        self.advance(seconds).await;
        let read = async || asked(READ) > before.0 && asked(READ_POSITION) > before.1;
        eventually("the Bridge never read Kitchen", WITHIN, read).await;
    }

    /// Moves time on by `seconds`, one second at a time, waiting each time until the Bridge
    /// read Kitchen afterwards: as real time passes near the end of an Item.
    async fn read_every_second(&self, seconds: u64) {
        for _ in 0..seconds {
            self.advance_and_read(1).await;
        }
    }

    /// Alice's Progress in the book, read from Audiobookshelf with her own key.
    async fn progress(&self) -> Option<(f64, bool)> {
        self.abs.item_progress(&self.alice, &self.book()).await
    }

    /// Waits until Alice's Progress in the book is `expected`.
    async fn wait_for_progress(&self, expected: (f64, bool)) {
        let what = format!("Progress never became {expected:?}");
        let current = async || self.progress().await == Some(expected);
        eventually(&what, WITHIN, current).await;
    }

    /// Bob skips Kitchen as `skip` says.
    async fn skip(&self, skip: Skip) -> Playback {
        let kitchen = self.kitchen.uuid();
        self.bridge
            .api()
            .skip_group(&self.bobs, kitchen, skip)
            .await
    }
}

fn seconds(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

fn playing(position: f64) -> Playback {
    Playback {
        state: TransportState::Playing,
        position: Some(position),
        duration: Some(LENGTH),
    }
}

#[tokio::test]
async fn a_book_starts_in_the_file_and_at_the_offset_of_its_progress() {
    let setup = setup().await;
    // 3000.4 seconds in: 600 into the second file.
    setup.start(Some(3000.4)).await;

    // Its three files are Kitchen's queue, in order, each its own URL on the one stream.
    let queue = setup.household.queue(&setup.kitchen);
    let names: Vec<&str> = queue
        .iter()
        .map(|url| url.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "1-01%20-%20Part%20One.wav",
            "2-02%20-%20Part%20Two.wav",
            "3-03%20-%20Part%20Three.wav",
        ]
    );
    let stream = |url: &str| url.rsplit('/').nth(1).unwrap().to_owned();
    assert!(queue.iter().all(|url| stream(url) == stream(&queue[0])));
    assert_eq!(setup.track(), (Some(2), seconds(600)));
    assert_eq!(setup.household.transport_state(&setup.kitchen), "PLAYING");
    let sought = [Seek::new("TRACK_NR", "2"), Seek::new("REL_TIME", "0:10:00")];
    assert_eq!(setup.household.sought(&setup.kitchen), sought);
    // It fetched the second file, the one it plays.
    let fetched = setup.household.fetched(&setup.kitchen).await;
    assert_eq!(fetched.uri, queue[1]);
    assert_eq!(fetched.status, Some(206));

    // Each file is served only under its own number and name.
    let api = setup.bridge.api();
    let path = |url: &str| url[url.find("/stream/").unwrap()..].to_owned();
    for url in &queue {
        let response = api.fetch(reqwest::Method::HEAD, &path(url), None).await;
        assert_eq!(response.status(), StatusCode::OK, "{url}");
    }
    let swapped = path(&queue[0]).replace("/1-01", "/2-01");
    let response = api.fetch(reqwest::Method::HEAD, &swapped, None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // On a file boundary, the later file from its start, without a seek into it.
    let book = setup.book();
    let abs = &setup.abs;
    abs.set_item_progress(&setup.alice, &book, 4200.0, false)
        .await;
    let office = setup.office.uuid();
    api.start_handoff(&setup.alices, &book, office).await;
    assert_eq!(setup.household.track_number(&setup.office), Some(3));
    let sought = setup.household.sought(&setup.office);
    assert_eq!(sought, [Seek::new("TRACK_NR", "3")]);
}

#[tokio::test]
async fn play_from_start_or_finished_starts_at_the_first_file() {
    let setup = setup().await;
    let book = setup.book();
    let api = setup.bridge.api();
    let abs = &setup.abs;
    abs.set_item_progress(&setup.alice, &book, 3000.0, false)
        .await;
    let kitchen = setup.kitchen.uuid();
    api.start_handoff_from_start(&setup.alices, &book, kitchen)
        .await;
    assert_eq!(setup.track(), (Some(1), Duration::ZERO));

    abs.set_item_progress(&setup.alice, &book, 3000.0, true)
        .await;
    api.start_handoff(&setup.alices, &book, kitchen).await;
    assert_eq!(setup.track(), (Some(1), Duration::ZERO));
    let sought = setup.household.sought(&setup.kitchen);
    assert!(
        sought.iter().all(|seek| seek.unit == "TRACK_NR"),
        "{sought:?}"
    );
}

#[tokio::test]
async fn progress_is_written_in_the_whole_book_while_a_later_file_plays() {
    let setup = setup().await;
    setup.start(Some(3000.0)).await;

    // The cadence writes where Kitchen is in the whole book, not in its file.
    setup.advance(30).await;
    setup.wait_for_progress((3030.0, false)).await;

    // So does a pause, which answers in the whole book too.
    setup.advance(5).await;
    let kitchen = setup.kitchen.uuid();
    let api = setup.bridge.api();
    let paused = api.pause_group(&setup.bobs, kitchen).await;
    let expected = Playback {
        state: TransportState::Paused,
        ..playing(3035.0)
    };
    assert_eq!(paused, expected);
    assert_eq!(setup.progress().await, Some((3035.0, false)));

    // The Groups list shows the Handoff on Kitchen.
    let groups = api.groups(&setup.bobs).await;
    let listed = groups.iter().find(|group| group.name == "Kitchen").unwrap();
    let handoff = listed.handoff.as_ref().unwrap();
    assert_eq!(handoff.item, setup.book());
}

#[tokio::test]
async fn the_book_plays_on_into_its_next_file_and_progress_follows() {
    let setup = setup().await;
    // 10 seconds before the end of the first file.
    setup.start(Some(2390.0)).await;

    setup.advance(30).await;
    assert_eq!(setup.track(), (Some(2), seconds(20)));
    setup.wait_for_progress((2420.0, false)).await;
    // The end of a file that isn't the last is not the end of the book.
    assert_eq!(setup.bridge.api().handoffs(&setup.bobs).await.len(), 1);

    // On into the last file too.
    setup.advance(1800).await;
    assert_eq!(setup.track(), (Some(3), seconds(20)));
    let writes_in_whole_book = async || {
        let progress = setup.progress().await;
        progress.is_some_and(|(time, finished)| time > 4200.0 && !finished)
    };
    eventually(
        "Progress never moved into the last file",
        WITHIN,
        writes_in_whole_book,
    )
    .await;
}

#[tokio::test]
async fn skips_across_file_boundaries_land_in_the_right_file() {
    let setup = setup().await;
    // 5 seconds into the second file.
    setup.start(Some(2405.0)).await;

    // 15 s back lands 10 s before the end of the first file.
    assert_eq!(setup.skip(Skip::Back15).await, playing(2390.0));
    assert_eq!(setup.track(), (Some(1), seconds(2390)));
    assert_eq!(setup.progress().await, Some((2390.0, false)));
    let sought = setup.household.sought(&setup.kitchen);
    let back = [Seek::new("TRACK_NR", "1"), Seek::new("REL_TIME", "0:39:50")];
    assert!(sought.ends_with(&back), "{sought:?}");

    // 30 s forward lands 20 s into the second.
    assert_eq!(setup.skip(Skip::Forward30).await, playing(2420.0));
    assert_eq!(setup.track(), (Some(2), seconds(20)));
    assert_eq!(setup.progress().await, Some((2420.0, false)));

    // Within a file, no file is selected.
    let before = setup.household.sought(&setup.kitchen).len();
    assert_eq!(setup.skip(Skip::Forward30).await, playing(2450.0));
    let sought = setup.household.sought(&setup.kitchen);
    assert_eq!(sought[before..], [Seek::new("REL_TIME", "0:00:50")]);

    // The book's length comes from its files', told at the start: with the Library Server
    // down, a forward skip still knows where the end is, and lands at the end of the last
    // file.
    setup.advance(4200 - 2450 + 95).await;
    assert_eq!(setup.track(), (Some(3), seconds(95)));
    setup.forwarder.stop().await;
    assert_eq!(setup.skip(Skip::Forward30).await, playing(LENGTH));
    let sought = setup.household.sought(&setup.kitchen);
    assert_eq!(sought.last(), Some(&Seek::new("REL_TIME", "0:02:00")));
}

#[tokio::test]
async fn the_group_stops_at_the_end_of_the_last_file_and_the_book_is_finished() {
    let setup = setup().await;
    // 90 seconds into the last file, 30 before the end of the book.
    setup.start(Some(4290.0)).await;

    // Read every second near the end, up to the end, where Kitchen stops.
    setup.advance_and_read(25).await;
    setup.read_every_second(5).await;
    assert_eq!(setup.household.transport_state(&setup.kitchen), "STOPPED");
    setup.wait_for_progress((LENGTH, true)).await;
    let api = setup.bridge.api();
    let ended = async || api.handoffs(&setup.bobs).await.is_empty();
    eventually("the Handoff never ended", WITHIN, ended).await;
}

#[tokio::test]
async fn previous_and_next_chapter_move_between_the_books_chapters() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let item = api
        .item(&setup.alices, &setup.book().library_item, None)
        .await;
    let starts: Vec<f64> = item.chapters.iter().map(|chapter| chapter.start).collect();
    assert_eq!(starts, [0.0, 2400.0, 4200.0]);
    // 5 seconds into the second chapter.
    setup.start(Some(2405.0)).await;

    // More than 3 seconds in: back to its start, in the second file.
    assert_eq!(setup.skip(Skip::PreviousChapter).await, playing(2400.0));
    assert_eq!(setup.track(), (Some(2), Duration::ZERO));
    assert_eq!(setup.progress().await, Some((2400.0, false)));
    // At its start: to the chapter before, in the first file.
    assert_eq!(setup.skip(Skip::PreviousChapter).await, playing(0.0));
    assert_eq!(setup.track(), (Some(1), Duration::ZERO));
    assert_eq!(setup.progress().await, Some((0.0, false)));
    // In the first chapter: its start.
    setup.advance(2).await;
    assert_eq!(setup.skip(Skip::PreviousChapter).await, playing(0.0));
    assert_eq!(setup.track(), (Some(1), Duration::ZERO));

    // Next, chapter by chapter, file by file.
    assert_eq!(setup.skip(Skip::NextChapter).await, playing(2400.0));
    assert_eq!(setup.track(), (Some(2), Duration::ZERO));
    assert_eq!(setup.skip(Skip::NextChapter).await, playing(4200.0));
    assert_eq!(setup.track(), (Some(3), Duration::ZERO));
    assert_eq!(setup.progress().await, Some((4200.0, false)));

    // In the last chapter, next sends nothing and writes nothing: the answer is where Kitchen
    // is.
    setup.advance(10).await;
    let before = setup.household.sought(&setup.kitchen);
    assert_eq!(setup.skip(Skip::NextChapter).await, playing(4210.0));
    assert_eq!(setup.household.sought(&setup.kitchen), before);
    assert_eq!(setup.track(), (Some(3), seconds(10)));
    assert_eq!(setup.progress().await, Some((4200.0, false)));

    // Chapters come from the Library Server: without it, a chapter skip is refused, and
    // nothing is sent.
    setup.forwarder.stop().await;
    let kitchen = setup.kitchen.uuid();
    let previous = GroupControl::Skip(Skip::PreviousChapter);
    assert_eq!(
        api.control_error(&setup.bobs, kitchen, previous).await,
        LIBRARY_SERVER_UNREACHABLE
    );
    assert_eq!(setup.household.sought(&setup.kitchen), before);
}

#[tokio::test]
async fn an_episode_counts_as_one_chapter() {
    let setup = setup().await;
    let api = setup.bridge.api();
    let episode = setup.abs.seeded().episode(0);
    let kitchen = setup.kitchen.uuid();
    api.start_handoff(&setup.alices, &episode, kitchen).await;
    setup.advance(10).await;
    let at = |position| Playback {
        duration: Some(600.0),
        ..playing(position)
    };

    let before = setup.household.sought(&setup.kitchen);
    assert_eq!(setup.skip(Skip::NextChapter).await, at(10.0));
    assert_eq!(setup.household.sought(&setup.kitchen), before);
    assert_eq!(setup.skip(Skip::PreviousChapter).await, at(0.0));
    assert_eq!(setup.household.position(&setup.kitchen), Duration::ZERO);
}

#[tokio::test]
async fn a_book_with_a_file_of_unknown_length_is_not_playable() {
    let abs = Audiobookshelf::shared();
    let proxy = LibraryProxy::to(abs.base_url()).await;
    let household = Household::with_manual_clock().0;
    let kitchen = household.add_player("Kitchen");
    let bridge = TestBridge::start(&home_with_sonos_toml(&proxy.url(), household.ssdp()));
    let user = abs.new_user().await;
    let session = bridge.api().login(&proxy.url(), &user.api_key.key).await;
    let book = ItemId {
        library_item: abs.seeded().long_book.clone(),
        episode: None,
    };

    proxy.hide_lengths();
    let api = bridge.api();
    assert_eq!(
        api.start_handoff_error(&session, &book, kitchen.uuid())
            .await,
        NOT_PLAYABLE
    );
    assert_eq!(
        household
            .actions_of(&kitchen)
            .iter()
            .filter(|a| a.contains("Queue"))
            .count(),
        0
    );
    assert_eq!(api.handoffs(&session).await, []);
}

#[tokio::test]
async fn a_book_with_a_file_in_a_format_sonos_doesnt_play_is_not_playable() {
    let abs = Audiobookshelf::shared();
    let mixed = abs.mixed_format_book().await;
    let household = Household::with_manual_clock().0;
    let kitchen = household.add_player("Kitchen");
    let bridge = TestBridge::start(&home_with_sonos_toml(abs.base_url(), household.ssdp()));
    let reader = abs.new_user_seeing(&[&mixed.library]).await;
    let session = bridge
        .api()
        .login(abs.base_url(), &reader.api_key.key)
        .await;
    let book = ItemId {
        library_item: mixed.book,
        episode: None,
    };

    let api = bridge.api();
    // Both files' lengths are known: only the second's format keeps the book off a Group.
    let item = api.item(&session, &book.library_item, None).await;
    let lengths: Vec<_> = item.audio_files.iter().map(|file| file.duration).collect();
    assert_eq!(lengths, [Some(60.0), Some(60.0)]);
    assert_eq!(
        api.start_handoff_error(&session, &book, kitchen.uuid())
            .await,
        NOT_PLAYABLE
    );
    assert_eq!(
        household
            .actions_of(&kitchen)
            .iter()
            .filter(|a| a.contains("Queue"))
            .count(),
        0
    );
    assert_eq!(api.handoffs(&session).await, []);
}
