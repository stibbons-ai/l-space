//! A fake Sonos household for tests (PRD "Testing Decisions"): players on loopback that answer
//! SSDP M-SEARCH and serve their device description and the UPnP subset the Bridge uses
//! (ZoneGroupTopology `GetZoneGroupState`; AVTransport `GetTransportInfo`, `GetPositionInfo`,
//! `SetAVTransportURI`, `Play`, `Pause` and `Seek`; GroupRenderingControl `GetGroupVolume`,
//! `SnapshotGroupVolume` and `SetGroupVolume`), with Groups, playback, volume and faults the
//! test controls.
//!
//! A Group told to play a URL (`SetAVTransportURI`, then `Play`) plays it like a speaker: its
//! transport is Playing, its position moves on the clock, `GetPositionInfo` reports the URL,
//! and it fetches the URL over HTTP with a `Range` request, recording what it got
//! ([`Household::fetched`]). Paused (`Pause`), its position stops on the clock; `Play` moves it
//! on from there; `Seek` (`REL_TIME`) sets it, playing or paused as it was. Each Group has a
//! volume, [`DEFAULT_VOLUME`] until set (`SetGroupVolume`, or [`Household::set_volume`]);
//! `SnapshotGroupVolume` is accepted and only recorded, as the fake has no member volumes.
//!
//! A track of known length plays to its end and stops there, as a Sonos Group stops at the end
//! of a single URL: Stopped, its track still loaded, its position back at 0. A URL the Group
//! is given has no known length (it plays on without end) unless the test sets one with
//! [`Household::set_url_length`].
//!
//! The household runs on its own Tokio runtime, so it answers whatever the test's runtime is
//! doing (also while a test blocks on starting a Bridge). Each player listens on its own
//! loopback port; one SSDP socket answers searches for all of them, as the players on a LAN
//! would each answer a multicast search. Point the Bridge's discovery at
//! [`Household::ssdp`].
//!
//! Playback positions advance on the [`Clock`] the household is started with, so a test with a
//! `clock::ManualClock` moves a Playing Group's position by advancing that clock.
//!
//! The household also serves that clock over HTTP ([`Household::clock_address`]), for a Bridge
//! in another process to follow (the Bridge's test-only clock hook): `GET /clock` answers how
//! far the clock has moved since the household started, in whole nanoseconds, as plain text;
//! `GET /clock?after=<nanoseconds>` answers the same once the clock has moved past `after` (or
//! after [`CLOCK_POLL`] in real time, unchanged). A follower that asks again with what it got
//! is waiting, caught up: [`Household::clock_followed`] waits for that after an advance.
//!
//! ```ignore
//! let clock = ManualClock::new();
//! let household = Household::start(Arc::new(clock.clone()));
//! let kitchen = household.add_player("Kitchen");
//! let bath = household.add_player("Bath");
//! household.join(&bath, &kitchen);            // one Group: Kitchen + Bath
//! household.play(&kitchen, FakeTrack::new("Moby Dick", Duration::from_secs(600)));
//! clock.advance(Duration::from_secs(30));     // Kitchen's position is now 0:00:30
//! ```

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use clock::{Clock, ManualClock};
use tokio::runtime::Runtime;

/// The `Range` a player asks for when it starts a URL: everything, from the start.
const FETCH_RANGE: &str = "bytes=0-";

/// How long `GET /clock?after=…` waits at most, in real time, for the clock to move.
pub const CLOCK_POLL: Duration = Duration::from_secs(10);

/// How long [`Household::clock_followed`] waits at most, in real time.
const FOLLOW_TIMEOUT: Duration = Duration::from_secs(10);

/// The most bytes of a URL's body a player keeps, per fetch; it reads no further.
pub const MAX_FETCHED: usize = 16 * 1024 * 1024;

/// How long a player's fetch may take in all.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

const ZONE_PLAYER: &str = "urn:schemas-upnp-org:device:ZonePlayer:1";
const TOPOLOGY: &str = "urn:schemas-upnp-org:service:ZoneGroupTopology:1";
const AV_TRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const GROUP_RENDERING: &str = "urn:schemas-upnp-org:service:GroupRenderingControl:1";

/// A Group's volume until it is set.
pub const DEFAULT_VOLUME: u8 = 20;

/// The highest volume `SetGroupVolume` takes; the lowest is 0.
pub const MAX_VOLUME: u8 = 100;

/// A player in the household, by its UUID.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlayerId(String);

impl PlayerId {
    /// Its UUID, which the Bridge reports as the member's (or its Group's) id.
    pub fn uuid(&self) -> &str {
        &self.0
    }
}

/// A track a Group plays.
#[derive(Debug, Clone, PartialEq)]
pub struct FakeTrack {
    pub title: Option<String>,
    pub creator: Option<String>,
    pub uri: String,
    /// None for a URL the Group was given, unless the test set a length for it
    /// ([`Household::set_url_length`]): it reports `0:00:00`, as for a stream, and the
    /// position moves on without end.
    pub duration: Option<Duration>,
}

impl FakeTrack {
    /// A track titled `title`, without a creator.
    pub fn new(title: &str, duration: Duration) -> Self {
        Self {
            title: Some(title.to_owned()),
            creator: None,
            uri: "http://127.0.0.1:1/track.mp3".to_owned(),
            duration: Some(duration),
        }
    }

    pub fn by(mut self, creator: &str) -> Self {
        self.creator = Some(creator.to_owned());
        self
    }
}

/// What a player got when it fetched the URL it was told to play.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetch {
    pub uri: String,
    /// The answer's status; none without an answer.
    pub status: Option<u16>,
    /// The answer's headers, names in lower case, in the order received.
    pub headers: Vec<(String, String)>,
    /// The body, up to [`MAX_FETCHED`] bytes.
    pub body: Vec<u8>,
    /// Why there was no answer, or the body broke off; none if all went well.
    pub error: Option<String>,
}

impl Fetch {
    /// The value of header `name` (lower case), if the answer had it.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }
}

/// How a player misbehaves over HTTP.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Fault {
    /// It answers correctly.
    #[default]
    None,
    /// It answers every SOAP call with `200 OK` and a body that isn't XML.
    Garbage,
    /// It accepts SOAP calls and never answers them (its device description still answers).
    Hang,
    /// It answers every SOAP call correctly, padded with whitespace inside the envelope to at
    /// least this many bytes.
    Padded(usize),
    /// It answers `GetPositionInfo` correctly, but only after this long (in real time, on the
    /// Tokio timer, not the household's clock).
    SlowPosition(Duration),
    /// It answers `GetTransportInfo` without a `CurrentTransportState`.
    NoTransportState,
    /// It answers `GetPositionInfo` with `NOT_IMPLEMENTED` for the position (`RelTime`), and
    /// everything else correctly.
    NoPosition,
    /// It answers `GetTransportInfo` with a SOAP fault, and everything else correctly.
    FailTransportInfo,
    /// It answers `SetAVTransportURI` and `Play` with a SOAP fault (UPnP error 714, illegal
    /// MIME type), and everything else correctly.
    RefusePlayback,
    /// It answers each `SetAVTransportURI` correctly, but only after this long (in real time),
    /// and takes the URL only then.
    SlowPlayback(Duration),
    /// It answers `Pause`, `Play`, `Seek`, `SnapshotGroupVolume` and `SetGroupVolume` with a
    /// SOAP fault (UPnP error 701, transition not available), changing nothing, and everything
    /// else correctly.
    RefuseControl,
    /// It accepts `Pause`, `Play`, `Seek`, `SnapshotGroupVolume` and `SetGroupVolume` and
    /// never answers them, changing nothing, and answers everything else correctly.
    HangControl,
    /// It answers `Seek` with a SOAP fault (UPnP error 701, transition not available),
    /// changing nothing, and everything else correctly.
    RefuseSeek,
    /// It accepts `Seek` and never answers it, changing nothing, and answers everything else
    /// correctly.
    HangSeek,
    /// It answers `Seek` as [`Fault::RefuseSeek`] does, and each `Pause` correctly, but only
    /// after this long (in real time), pausing only then; everything else correctly.
    RefuseSeekSlowPause(Duration),
    /// It answers every AVTransport call correctly, but only after this long (in real time),
    /// and acts only then.
    SlowTransport(Duration),
    /// It answers `GetGroupVolume` with a SOAP fault, and everything else correctly.
    FailGroupVolume,
}

/// A fake Sonos household; dropping it stops every player.
#[derive(Debug)]
pub struct Household {
    shared: Arc<Shared>,
    ssdp: SocketAddr,
    clock: SocketAddr,
    runtime: Option<Runtime>,
}

#[derive(Debug)]
struct Shared {
    clock: Arc<dyn Clock>,
    /// When the household started, on `clock`: what `GET /clock` counts from.
    origin: Instant,
    /// Woken each time a `GET /clock?after=…` starts waiting.
    clock_waiting: tokio::sync::Notify,
    state: Mutex<HouseholdState>,
    /// How players fetch the URLs they play.
    http: reqwest::Client,
}

#[derive(Debug, Default)]
struct HouseholdState {
    /// In the order they were added.
    players: Vec<Player>,
    /// Every SOAP action called, as the UUID of the player called and `Service#Action`.
    actions: Vec<(String, String)>,
    /// Every fetch of a URL a Group played, done, as the UUID of the coordinator and what it
    /// got.
    fetches: Vec<(String, Fetch)>,
    /// Every `CurrentURI` a player was given with `SetAVTransportURI`, as the UUID of the
    /// player and the URL, recorded on arrival, also when it then misbehaves.
    offered: Vec<(String, String)>,
    /// How many M-SEARCHes for ZonePlayers were received.
    searches: usize,
    added: u32,
    /// The `after` of each `GET /clock?after=…` waiting for an answer.
    clock_waits: Vec<u64>,
}

#[derive(Debug)]
struct Player {
    uuid: String,
    name: String,
    addr: SocketAddr,
    /// The player it is bonded to, whose Group it follows, for a speaker the Sonos app doesn't
    /// show (`Invisible="1"`).
    bonded_to: Option<String>,
    /// The UUID of its Group's coordinator.
    group: String,
    transport: Transport,
    /// Its Group's volume, while it coordinates the Group.
    volume: u8,
    fault: Fault,
    /// The length of each URL it is given from now on, if the test set one.
    url_length: Option<Duration>,
}

#[derive(Debug)]
struct Transport {
    /// As AVTransport names it, e.g. `PAUSED_PLAYBACK`.
    state: &'static str,
    track: Option<FakeTrack>,
    /// The position when playback last started or stopped moving.
    offset: Duration,
    /// When playback last started, while it is playing.
    since: Option<Instant>,
}

impl Transport {
    fn position(&self, now: Instant) -> Duration {
        let moved = self
            .since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        let position = self.offset + moved;
        match &self.track {
            Some(track) => track.duration.map_or(position, |end| position.min(end)),
            None => Duration::ZERO,
        }
    }

    /// Stops a track that has played to its end by `now`, as a Group does at the end of a
    /// single URL: Stopped, its track still loaded, back at its start.
    fn settle(&mut self, now: Instant) {
        let end = self.track.as_ref().and_then(|track| track.duration);
        if let (Some(since), Some(end)) = (self.since, end)
            && self.offset + now.saturating_duration_since(since) >= end
        {
            self.state = "STOPPED";
            self.offset = Duration::ZERO;
            self.since = None;
        }
    }
}

impl Household {
    /// A household without players, its SSDP socket on loopback; positions advance on `clock`.
    pub fn start(clock: Arc<dyn Clock>) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("sonos-fake runtime");
        let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind SSDP");
        socket.set_nonblocking(true).expect("non-blocking SSDP");
        let ssdp = socket.local_addr().expect("SSDP address");
        let http = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .no_proxy()
            .build()
            .expect("an HTTP client without TLS builds");
        let shared = Arc::new(Shared {
            origin: clock.now(),
            clock_waiting: tokio::sync::Notify::new(),
            clock,
            state: Mutex::default(),
            http,
        });
        let clock_listener =
            std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind the clock");
        clock_listener
            .set_nonblocking(true)
            .expect("non-blocking clock");
        let clock = clock_listener.local_addr().expect("clock address");
        {
            let _runtime = runtime.enter();
            let socket = tokio::net::UdpSocket::from_std(socket).expect("SSDP socket");
            runtime.spawn(answer_searches(Arc::clone(&shared), socket));
            let listener = tokio::net::TcpListener::from_std(clock_listener).expect("clock socket");
            let router = Router::new()
                .route("/clock", get(serve_clock))
                .with_state(Arc::clone(&shared));
            runtime.spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
        }
        Self {
            shared,
            ssdp,
            clock,
            runtime: Some(runtime),
        }
    }

    /// [`Household::start`] on a new `clock::ManualClock`, which it returns for the test to
    /// advance.
    pub fn with_manual_clock() -> (Self, ManualClock) {
        let clock = ManualClock::new();
        (Self::start(Arc::new(clock.clone())), clock)
    }

    /// Where the household answers SSDP searches, for the Bridge's discovery.
    pub fn ssdp(&self) -> SocketAddr {
        self.ssdp
    }

    /// Where the household serves its clock (`GET /clock`), for a Bridge's test clock hook.
    pub fn clock_address(&self) -> SocketAddr {
        self.clock
    }

    /// Waits until a follower of the clock has seen its current time and asks for the next
    /// one (`GET /clock?after=<now>`), at most [`FOLLOW_TIMEOUT`] in real time. Panics if none
    /// does by then. After advancing the clock, a test waits for this before it expects a
    /// Bridge following the clock to act on the new time.
    pub async fn clock_followed(&self) {
        let deadline = tokio::time::Instant::now() + FOLLOW_TIMEOUT;
        loop {
            // Enabled before checking, so a follower arriving in between still wakes it.
            let waiting = self.shared.clock_waiting.notified();
            let mut waiting = std::pin::pin!(waiting);
            waiting.as_mut().enable();
            let now = self.shared.elapsed();
            if self.shared.lock().clock_waits.contains(&now) {
                return;
            }
            let woken = tokio::time::timeout_at(deadline, waiting).await;
            assert!(woken.is_ok(), "nobody followed the clock to {now} ns");
        }
    }

    /// Adds a player named `name` in a Group of its own, stopped with nothing loaded.
    pub fn add_player(&self, name: &str) -> PlayerId {
        self.add(name, None)
    }

    /// Adds a speaker bonded to `main` (a surround or sub): in `main`'s Group, named like it,
    /// and invisible in the Sonos app.
    pub fn add_bonded(&self, main: &PlayerId) -> PlayerId {
        self.add("", Some(main))
    }

    fn add(&self, name: &str, bonded_to: Option<&PlayerId>) -> PlayerId {
        let listener =
            std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind a player");
        listener.set_nonblocking(true).expect("non-blocking player");
        let addr = listener.local_addr().expect("player address");
        let uuid = {
            let mut state = self.shared.lock();
            state.added += 1;
            let uuid = format!("RINCON_FAKE{:06}01400", state.added);
            let (name, group) = match bonded_to {
                Some(main) => {
                    let main = state.player(main);
                    (main.name.clone(), main.group.clone())
                }
                None => (name.to_owned(), uuid.clone()),
            };
            state.players.push(Player {
                uuid: uuid.clone(),
                name,
                addr,
                bonded_to: bonded_to.map(|main| main.0.clone()),
                group,
                transport: Transport {
                    state: "STOPPED",
                    track: None,
                    offset: Duration::ZERO,
                    since: None,
                },
                volume: DEFAULT_VOLUME,
                fault: Fault::None,
                url_length: None,
            });
            uuid
        };
        let runtime = self.runtime();
        let router = Router::new()
            .route("/xml/device_description.xml", get(description))
            .route("/ZoneGroupTopology/Control", post(control))
            .route("/MediaRenderer/AVTransport/Control", post(control))
            .route(
                "/MediaRenderer/GroupRenderingControl/Control",
                post(control),
            )
            .with_state(Served {
                shared: Arc::clone(&self.shared),
                uuid: uuid.clone(),
            });
        let _runtime = runtime.enter();
        let listener = tokio::net::TcpListener::from_std(listener).expect("player socket");
        runtime.spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        PlayerId(uuid)
    }

    /// Moves `member` (with the speakers bonded to it) into `coordinator`'s Group. `member`
    /// must not coordinate other players.
    pub fn join(&self, member: &PlayerId, coordinator: &PlayerId) {
        let mut state = self.shared.lock();
        let coordinates_others = state.players.iter().any(|player| {
            player.group == member.0 && player.uuid != member.0 && !player.follows(&member.0)
        });
        assert!(
            !coordinates_others,
            "{} coordinates other players",
            member.0
        );
        let group = state.player(coordinator).group.clone();
        state.regroup(&member.0, &group);
    }

    /// Takes `member` (with the speakers bonded to it) out of its Group into one of its own.
    pub fn leave(&self, member: &PlayerId) {
        self.shared.lock().regroup(&member.0, &member.0);
    }

    /// `coordinator`'s Group starts playing `track` from its start.
    pub fn play(&self, coordinator: &PlayerId, track: FakeTrack) {
        let now = self.shared.clock.now();
        let mut state = self.shared.lock();
        state.coordinator_mut(coordinator).transport = Transport {
            state: "PLAYING",
            track: Some(track),
            offset: Duration::ZERO,
            since: Some(now),
        };
    }

    /// `coordinator`'s Group pauses where it is.
    pub fn pause(&self, coordinator: &PlayerId) {
        self.settled(coordinator, |transport, now| {
            transport.offset = transport.position(now);
            transport.since = None;
            transport.state = "PAUSED_PLAYBACK";
        });
    }

    /// `coordinator`'s Group stops; its track stays loaded, back at its start.
    pub fn stop(&self, coordinator: &PlayerId) {
        let mut state = self.shared.lock();
        let transport = &mut state.coordinator_mut(coordinator).transport;
        transport.offset = Duration::ZERO;
        transport.since = None;
        transport.state = "STOPPED";
    }

    /// Each URL `coordinator` is given from now on (`SetAVTransportURI`) is `length` long, as
    /// `GetPositionInfo` then tells: played to its end, the Group stops.
    pub fn set_url_length(&self, coordinator: &PlayerId, length: Duration) {
        self.shared.lock().player_mut(coordinator).url_length = Some(length);
    }

    /// `coordinator`'s Group reports `state` as its `CurrentTransportState`, as is, such as
    /// `TRANSITIONING`, or one the Bridge doesn't know.
    pub fn set_transport_state(&self, coordinator: &PlayerId, state: &'static str) {
        self.shared
            .lock()
            .coordinator_mut(coordinator)
            .transport
            .state = state;
    }

    /// `player` misbehaves as `fault` from now on.
    pub fn set_fault(&self, player: &PlayerId, fault: Fault) {
        self.shared.lock().player_mut(player).fault = fault;
    }

    /// Takes `player` (with the speakers bonded to it) out of the household, as if unplugged:
    /// it no longer answers searches or calls, and players in its Group get Groups of their
    /// own.
    pub fn remove(&self, player: &PlayerId) {
        let mut state = self.shared.lock();
        state
            .players
            .retain(|other| other.uuid != player.0 && !other.follows(&player.0));
        for other in &mut state.players {
            if other.group == player.0 {
                other.group = other
                    .bonded_to
                    .clone()
                    .unwrap_or_else(|| other.uuid.clone());
            }
        }
    }

    /// Every SOAP action the household was called with, in order, as `Service#Action`, such as
    /// `ZoneGroupTopology#GetZoneGroupState`.
    pub fn actions(&self) -> Vec<String> {
        let state = self.shared.lock();
        state
            .actions
            .iter()
            .map(|(_, action)| action.clone())
            .collect()
    }

    /// The SOAP actions `player` was called with, in order, as for [`Household::actions`].
    pub fn actions_of(&self, player: &PlayerId) -> Vec<String> {
        let state = self.shared.lock();
        let actions = state.actions.iter().filter(|(uuid, _)| *uuid == player.0);
        actions.map(|(_, action)| action.clone()).collect()
    }

    /// Every URL `player` was given with `SetAVTransportURI`, in order, also those it then
    /// refused or never answered.
    pub fn offered(&self, player: &PlayerId) -> Vec<String> {
        let state = self.shared.lock();
        let offered = state.offered.iter().filter(|(uuid, _)| *uuid == player.0);
        offered.map(|(_, uri)| uri.clone()).collect()
    }

    /// How many M-SEARCHes for ZonePlayers the household has received, counted as each
    /// arrives (before it is answered).
    pub fn searches(&self) -> usize {
        self.shared.lock().searches
    }

    /// The URL `coordinator`'s Group has loaded, as `GetPositionInfo` reports it; none if
    /// nothing is loaded.
    pub fn uri(&self, coordinator: &PlayerId) -> Option<String> {
        let state = self.shared.lock();
        let track = state.coordinator(coordinator).transport.track.as_ref();
        track.map(|track| track.uri.clone())
    }

    /// `coordinator`'s Group's `CurrentTransportState`, such as `PLAYING`.
    pub fn transport_state(&self, coordinator: &PlayerId) -> &'static str {
        self.settled(coordinator, |transport, _| transport.state)
    }

    /// How far into its track `coordinator`'s Group is, on the household's clock.
    pub fn position(&self, coordinator: &PlayerId) -> Duration {
        self.settled(coordinator, |transport, now| transport.position(now))
    }

    /// What `act` does with `coordinator`'s Group's transport at the household's now, the
    /// transport first stopped if it played to its end by then.
    fn settled<T>(
        &self,
        coordinator: &PlayerId,
        act: impl FnOnce(&mut Transport, Instant) -> T,
    ) -> T {
        let now = self.shared.clock.now();
        let mut state = self.shared.lock();
        let transport = &mut state.coordinator_mut(coordinator).transport;
        transport.settle(now);
        act(transport, now)
    }

    /// `coordinator`'s Group's volume, as `GetGroupVolume` reports it.
    pub fn volume(&self, coordinator: &PlayerId) -> u8 {
        self.shared.lock().coordinator(coordinator).volume
    }

    /// `coordinator`'s Group's volume becomes `volume`, as if set in the Sonos app.
    pub fn set_volume(&self, coordinator: &PlayerId, volume: u8) {
        self.shared.lock().coordinator_mut(coordinator).volume = volume;
    }

    /// The first fetch `coordinator` finished of a URL its Group was told to play (with
    /// `Range: bytes=0-`), waiting for it up to twice a fetch's own time limit. Panics if there
    /// is none by then.
    pub async fn fetched(&self, coordinator: &PlayerId) -> Fetch {
        let deadline = Instant::now() + 2 * FETCH_TIMEOUT;
        loop {
            let first = {
                let state = self.shared.lock();
                let mut fetches = state.fetches.iter();
                let first = fetches.find(|(uuid, _)| *uuid == coordinator.0);
                first.map(|(_, fetch)| fetch.clone())
            };
            if let Some(fetch) = first {
                return fetch;
            }
            assert!(
                Instant::now() < deadline,
                "{} fetched nothing",
                coordinator.0
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn runtime(&self) -> &Runtime {
        self.runtime.as_ref().expect("running until dropped")
    }
}

impl Drop for Household {
    fn drop(&mut self) {
        // Not `drop`: that blocks, which a test's async context forbids.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, HouseholdState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How far the clock has moved since the household started, in whole nanoseconds.
    fn elapsed(&self) -> u64 {
        let elapsed = self.clock.now().saturating_duration_since(self.origin);
        u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
    }
}

/// Answers `GET /clock`: see the module documentation.
async fn serve_clock(State(shared): State<Arc<Shared>>, RawQuery(query): RawQuery) -> Response {
    let after = query.as_deref().and_then(|query| {
        let after = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("after="));
        after.and_then(|after| after.parse::<u64>().ok())
    });
    if let Some(after) = after {
        let waiting = ClockWait::new(Arc::clone(&shared), after);
        let moved = shared.origin + Duration::from_nanos(after.saturating_add(1));
        let _ = tokio::time::timeout(CLOCK_POLL, shared.clock.sleep_until(moved)).await;
        drop(waiting);
    }
    shared.elapsed().to_string().into_response()
}

/// A `GET /clock?after=…` waiting, recorded in [`HouseholdState::clock_waits`] while it lives
/// (also when its client hangs up, which drops it).
struct ClockWait {
    shared: Arc<Shared>,
    after: u64,
}

impl ClockWait {
    fn new(shared: Arc<Shared>, after: u64) -> Self {
        shared.lock().clock_waits.push(after);
        shared.clock_waiting.notify_waiters();
        Self { shared, after }
    }
}

impl Drop for ClockWait {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        if let Some(found) = state
            .clock_waits
            .iter()
            .position(|&after| after == self.after)
        {
            state.clock_waits.swap_remove(found);
        }
    }
}

impl HouseholdState {
    fn player(&self, id: &PlayerId) -> &Player {
        self.find(&id.0)
            .unwrap_or_else(|| panic!("no player {}", id.0))
    }

    fn player_mut(&mut self, id: &PlayerId) -> &mut Player {
        self.find_mut(&id.0)
            .unwrap_or_else(|| panic!("no player {}", id.0))
    }

    /// `id`, which must coordinate its Group: the Bridge reads a Group's playback from there.
    fn coordinator_mut(&mut self, id: &PlayerId) -> &mut Player {
        let player = self.player_mut(id);
        assert_eq!(player.group, id.0, "{} does not coordinate its Group", id.0);
        player
    }

    fn coordinator(&self, id: &PlayerId) -> &Player {
        let player = self.player(id);
        assert_eq!(player.group, id.0, "{} does not coordinate its Group", id.0);
        player
    }

    fn find(&self, uuid: &str) -> Option<&Player> {
        self.players.iter().find(|player| player.uuid == uuid)
    }

    fn find_mut(&mut self, uuid: &str) -> Option<&mut Player> {
        self.players.iter_mut().find(|player| player.uuid == uuid)
    }

    /// Puts `uuid` and the speakers bonded to it into the Group coordinated by `group`.
    fn regroup(&mut self, uuid: &str, group: &str) {
        for player in &mut self.players {
            if player.uuid == uuid || player.follows(uuid) {
                player.group = group.to_owned();
            }
        }
    }

    /// `served`'s player, if it coordinates its Group; UPnP error 701 (transition not
    /// available) otherwise.
    fn coordinating_mut(&mut self, served: &Served) -> Result<&mut Player, u16> {
        let player = self.find_mut(&served.uuid).ok_or(701u16)?;
        if player.group != served.uuid {
            return Err(701);
        }
        Ok(player)
    }

    /// `SetAVTransportURI`, `Play`, `Pause` or `Seek` (with the SOAP `request`) on `served`'s
    /// player, which must coordinate its Group, at `now`. `Play` on a stopped Group starts
    /// fetching the URL; on a paused one it moves on from where it is. `Pause` on a paused
    /// Group changes nothing. The UPnP error code on failure: 701 (transition not available)
    /// on a player that doesn't coordinate, with nothing loaded, or pausing a stopped Group;
    /// 402 (invalid arguments) without a `CurrentURI`, or a seek that isn't `REL_TIME` to
    /// `H:MM:SS`; 711 (illegal seek target) past the track's end.
    fn transport(
        &mut self,
        served: &Served,
        action: &str,
        request: &str,
        now: Instant,
    ) -> Result<(), u16> {
        let player = self.coordinating_mut(served)?;
        let url_length = player.url_length;
        let transport = &mut player.transport;
        match action {
            "SetAVTransportURI" => {
                let uri = input(request, "CurrentURI").ok_or(402u16)?;
                *transport = Transport {
                    state: "STOPPED",
                    track: Some(FakeTrack {
                        title: None,
                        creator: None,
                        uri,
                        duration: url_length,
                    }),
                    offset: Duration::ZERO,
                    since: None,
                };
            }
            "Play" => {
                let uri = transport.track.as_ref().ok_or(701u16)?.uri.clone();
                let stopped = transport.state == "STOPPED";
                if transport.since.is_none() {
                    transport.since = Some(now);
                }
                transport.state = "PLAYING";
                // A speaker fetches a URL it starts, not one it resumes.
                if stopped {
                    tokio::spawn(fetch(Arc::clone(&served.shared), served.uuid.clone(), uri));
                }
            }
            "Pause" => match transport.state {
                "PLAYING" => {
                    transport.offset = transport.position(now);
                    transport.since = None;
                    transport.state = "PAUSED_PLAYBACK";
                }
                "PAUSED_PLAYBACK" => {}
                _ => return Err(701),
            },
            "Seek" => {
                let track = transport.track.as_ref().ok_or(701u16)?;
                if input(request, "Unit").as_deref() != Some("REL_TIME") {
                    return Err(402);
                }
                let target = input(request, "Target")
                    .as_deref()
                    .and_then(parse_upnp_time)
                    .ok_or(402u16)?;
                if track.duration.is_some_and(|end| target > end) {
                    return Err(711);
                }
                transport.offset = target;
                if transport.since.is_some() {
                    transport.since = Some(now);
                }
            }
            _ => return Err(401),
        }
        Ok(())
    }

    /// The household's `ZoneGroupState` document: every Group, its members in the order they
    /// were added.
    fn zone_group_state(&self) -> String {
        let mut coordinators: Vec<&str> = Vec::new();
        for player in &self.players {
            if !coordinators.contains(&player.group.as_str()) {
                coordinators.push(&player.group);
            }
        }
        let mut xml = String::from("<ZoneGroupState><ZoneGroups>");
        for coordinator in coordinators {
            xml.push_str(&format!(
                r#"<ZoneGroup Coordinator="{coordinator}" ID="{coordinator}:1">"#
            ));
            let members = self
                .players
                .iter()
                .filter(|player| player.group == coordinator);
            for player in members {
                xml.push_str(&format!(
                    r#"<ZoneGroupMember UUID="{}" Location="{}" ZoneName="{}"{}/>"#,
                    player.uuid,
                    location(player.addr),
                    escape(&player.name),
                    if player.bonded_to.is_some() {
                        r#" Invisible="1""#
                    } else {
                        ""
                    },
                ));
            }
            xml.push_str("</ZoneGroup>");
        }
        xml.push_str("</ZoneGroups><VanishedDevices></VanishedDevices></ZoneGroupState>");
        xml
    }
}

impl Player {
    /// Whether this is a speaker bonded to `uuid`.
    fn follows(&self, uuid: &str) -> bool {
        self.bonded_to.as_deref() == Some(uuid)
    }
}

fn location(addr: SocketAddr) -> String {
    format!("http://{addr}/xml/device_description.xml")
}

/// The text of input argument `name` in a SOAP `request`, if it has one.
fn input(request: &str, name: &str) -> Option<String> {
    let document = roxmltree::Document::parse(request).ok()?;
    let argument = document
        .descendants()
        .find(|node| node.is_element() && node.tag_name().name() == name)?;
    Some(argument.text().unwrap_or_default().to_owned())
}

/// Fetches `uri` as player `uuid` does when told to play it, and records what it got.
async fn fetch(shared: Arc<Shared>, uuid: String, uri: String) {
    let mut fetch = Fetch {
        uri: uri.clone(),
        status: None,
        headers: Vec::new(),
        body: Vec::new(),
        error: None,
    };
    match shared
        .http
        .get(&uri)
        .header("range", FETCH_RANGE)
        .send()
        .await
    {
        Ok(mut response) => {
            fetch.status = Some(response.status().as_u16());
            fetch.headers = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
                    (name.as_str().to_owned(), value)
                })
                .collect();
            while fetch.body.len() < MAX_FETCHED {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        let room = MAX_FETCHED - fetch.body.len();
                        fetch
                            .body
                            .extend_from_slice(&chunk[..chunk.len().min(room)]);
                    }
                    Ok(None) => break,
                    Err(error) => {
                        fetch.error = Some(error.to_string());
                        break;
                    }
                }
            }
        }
        Err(error) => fetch.error = Some(error.to_string()),
    }
    shared.lock().fetches.push((uuid, fetch));
}

/// Answers each M-SEARCH for ZonePlayers (or everything) with one answer per player.
async fn answer_searches(shared: Arc<Shared>, socket: tokio::net::UdpSocket) {
    let mut buffer = [0; 2048];
    while let Ok((length, from)) = socket.recv_from(&mut buffer).await {
        let search = String::from_utf8_lossy(&buffer[..length]);
        let wanted = search.starts_with("M-SEARCH")
            && search.lines().any(|line| {
                let line = line.to_ascii_uppercase();
                line.starts_with("ST:")
                    && (line.contains(&ZONE_PLAYER.to_ascii_uppercase())
                        || line.contains("SSDP:ALL"))
            });
        if !wanted {
            continue;
        }
        shared.lock().searches += 1;
        let answers: Vec<String> = shared
            .lock()
            .players
            .iter()
            .map(|player| {
                format!(
                    "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age = 1800\r\nEXT:\r\nLOCATION: {}\r\nSERVER: Linux UPnP/1.0 Sonos/97.2-81220 (sonos-fake)\r\nST: {ZONE_PLAYER}\r\nUSN: uuid:{}::{ZONE_PLAYER}\r\n\r\n",
                    location(player.addr),
                    player.uuid,
                )
            })
            .collect();
        for answer in answers {
            let _ = socket.send_to(answer.as_bytes(), from).await;
        }
    }
}

/// What a player's HTTP server serves from.
#[derive(Debug, Clone)]
struct Served {
    shared: Arc<Shared>,
    uuid: String,
}

async fn description(State(served): State<Served>) -> Response {
    let name = served
        .shared
        .lock()
        .find(&served.uuid)
        .map(|player| escape(&player.name))
        .unwrap_or_default();
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><root xmlns="urn:schemas-upnp-org:device-1-0"><specVersion><major>1</major><minor>0</minor></specVersion><device><deviceType>{ZONE_PLAYER}</deviceType><friendlyName>127.0.0.1 - sonos-fake</friendlyName><manufacturer>Sonos, Inc.</manufacturer><modelName>sonos-fake</modelName><UDN>uuid:{}</UDN><roomName>{name}</roomName></device></root>"#,
        served.uuid
    );
    xml(StatusCode::OK, body)
}

/// Answers a SOAP call on any service by its `SOAPACTION` header.
async fn control(State(served): State<Served>, headers: HeaderMap, request: String) -> Response {
    let soap_action = headers
        .get("soapaction")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim_matches('"');
    let (urn, action) = soap_action.split_once('#').unwrap_or(("", soap_action));
    let service = urn.rsplit(':').nth(1).unwrap_or_default();
    // When the call came: what it answers and does then, unless a fault delays it.
    let (fault, received) = {
        let mut state = served.shared.lock();
        let received = served.shared.clock.now();
        state
            .actions
            .push((served.uuid.clone(), format!("{service}#{action}")));
        if action == "SetAVTransportURI"
            && let Some(uri) = input(&request, "CurrentURI")
        {
            state.offered.push((served.uuid.clone(), uri));
        }
        (
            state.find(&served.uuid).map(|player| player.fault),
            received,
        )
    };
    // Whether a fault held the call up: it then answers and acts as of when it is let go.
    let delayed = match fault {
        Some(Fault::Hang) => std::future::pending().await,
        Some(Fault::Garbage) => return xml(StatusCode::OK, "<s:Envelope><unclosed".to_owned()),
        Some(Fault::SlowPosition(delay)) if action == "GetPositionInfo" => {
            tokio::time::sleep(delay).await;
            true
        }
        Some(Fault::FailTransportInfo) if action == "GetTransportInfo" => return invalid_action(),
        Some(Fault::RefusePlayback) if matches!(action, "SetAVTransportURI" | "Play") => {
            return upnp_fault(714);
        }
        Some(Fault::SlowPlayback(delay)) if action == "SetAVTransportURI" => {
            tokio::time::sleep(delay).await;
            true
        }
        Some(Fault::RefuseControl)
            if matches!(
                action,
                "Pause" | "Play" | "Seek" | "SnapshotGroupVolume" | "SetGroupVolume"
            ) =>
        {
            return upnp_fault(701);
        }
        Some(Fault::HangControl)
            if matches!(
                action,
                "Pause" | "Play" | "Seek" | "SnapshotGroupVolume" | "SetGroupVolume"
            ) =>
        {
            std::future::pending().await
        }
        Some(Fault::RefuseSeek | Fault::RefuseSeekSlowPause(_)) if action == "Seek" => {
            return upnp_fault(701);
        }
        Some(Fault::RefuseSeekSlowPause(delay)) if action == "Pause" => {
            tokio::time::sleep(delay).await;
            true
        }
        Some(Fault::HangSeek) if action == "Seek" => std::future::pending().await,
        Some(Fault::FailGroupVolume) if action == "GetGroupVolume" => return invalid_action(),
        Some(Fault::SlowTransport(delay)) if urn == AV_TRANSPORT => {
            tokio::time::sleep(delay).await;
            true
        }
        Some(
            Fault::None
            | Fault::Padded(_)
            | Fault::SlowPosition(_)
            | Fault::NoTransportState
            | Fault::NoPosition
            | Fault::FailTransportInfo
            | Fault::RefusePlayback
            | Fault::SlowPlayback(_)
            | Fault::RefuseControl
            | Fault::HangControl
            | Fault::RefuseSeek
            | Fault::HangSeek
            | Fault::RefuseSeekSlowPause(_)
            | Fault::FailGroupVolume
            | Fault::SlowTransport(_),
        ) => false,
        // Removed from the household: as good as unplugged.
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let now = if delayed {
        served.shared.clock.now()
    } else {
        received
    };
    let mut state = served.shared.lock();
    for player in &mut state.players {
        player.transport.settle(now);
    }
    let arguments: Vec<(&str, String)> = match (urn, action) {
        (TOPOLOGY, "GetZoneGroupState") => {
            vec![("ZoneGroupState", state.zone_group_state())]
        }
        (AV_TRANSPORT, "GetTransportInfo") | (AV_TRANSPORT, "GetPositionInfo") => {
            let Some(transport) = state
                .find(&served.uuid)
                .and_then(|player| state.find(&player.group))
                .map(|coordinator| &coordinator.transport)
            else {
                return invalid_action();
            };
            if action == "GetTransportInfo" {
                let mut arguments = vec![
                    ("CurrentTransportStatus", "OK".to_owned()),
                    ("CurrentSpeed", "1".to_owned()),
                ];
                if fault != Some(Fault::NoTransportState) {
                    arguments.insert(0, ("CurrentTransportState", transport.state.to_owned()));
                }
                arguments
            } else {
                let mut arguments = position_info(transport, now);
                if fault == Some(Fault::NoPosition) {
                    for (name, value) in &mut arguments {
                        if *name == "RelTime" {
                            "NOT_IMPLEMENTED".clone_into(value);
                        }
                    }
                }
                arguments
            }
        }
        (AV_TRANSPORT, "SetAVTransportURI" | "Play" | "Pause" | "Seek") => {
            match state.transport(&served, action, &request, now) {
                Ok(()) => Vec::new(),
                Err(code) => return upnp_fault(code),
            }
        }
        (GROUP_RENDERING, "GetGroupVolume") => match state.coordinating_mut(&served) {
            Ok(coordinator) => vec![("CurrentVolume", coordinator.volume.to_string())],
            Err(code) => return upnp_fault(code),
        },
        (GROUP_RENDERING, "SnapshotGroupVolume") => match state.coordinating_mut(&served) {
            Ok(_) => Vec::new(),
            Err(code) => return upnp_fault(code),
        },
        (GROUP_RENDERING, "SetGroupVolume") => {
            let Some(volume) = input(&request, "DesiredVolume")
                .and_then(|volume| volume.parse::<u8>().ok())
                .filter(|volume| *volume <= MAX_VOLUME)
            else {
                return upnp_fault(402);
            };
            match state.coordinating_mut(&served) {
                Ok(coordinator) => coordinator.volume = volume,
                Err(code) => return upnp_fault(code),
            }
            Vec::new()
        }
        _ => return invalid_action(),
    };
    drop(state);
    let mut body = String::new();
    for (name, value) in arguments {
        body.push_str(&format!("<{name}>{}</{name}>", escape(&value)));
    }
    let mut envelope = format!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action}Response xmlns:u="{urn}">{body}</u:{action}Response></s:Body></s:Envelope>"#
    );
    if let Some(Fault::Padded(length)) = fault {
        // Whitespace inside the envelope keeps the answer valid however long it gets.
        let padding = " ".repeat(length.saturating_sub(envelope.len()));
        envelope = envelope.replacen("<s:Body>", &format!("<s:Body>{padding}"), 1);
    }
    xml(StatusCode::OK, envelope)
}

fn position_info(transport: &Transport, now: Instant) -> Vec<(&'static str, String)> {
    let (track, duration, metadata, uri) = match &transport.track {
        Some(track) => {
            let mut didl = String::from(
                r#"<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"><item id="-1" parentID="-1" restricted="true">"#,
            );
            if let Some(title) = &track.title {
                didl.push_str(&format!("<dc:title>{}</dc:title>", escape(title)));
            }
            if let Some(creator) = &track.creator {
                didl.push_str(&format!("<dc:creator>{}</dc:creator>", escape(creator)));
            }
            didl.push_str(
                "<upnp:class>object.item.audioItem.musicTrack</upnp:class></item></DIDL-Lite>",
            );
            let duration = upnp_time(track.duration.unwrap_or_default());
            ("1", duration, didl, track.uri.clone())
        }
        None => ("0", upnp_time(Duration::ZERO), String::new(), String::new()),
    };
    vec![
        ("Track", track.to_owned()),
        ("TrackDuration", duration),
        ("TrackMetaData", metadata),
        ("TrackURI", uri),
        ("RelTime", upnp_time(transport.position(now))),
        ("AbsTime", "NOT_IMPLEMENTED".to_owned()),
        ("RelCount", "2147483647".to_owned()),
        ("AbsCount", "2147483647".to_owned()),
    ]
}

/// `H:MM:SS` (any number of hours), as a Sonos player takes a `REL_TIME` seek target; none if
/// it isn't one.
fn parse_upnp_time(time: &str) -> Option<Duration> {
    let mut parts = time.split(':');
    let (hours, minutes, seconds) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || minutes.len() != 2 || seconds.len() != 2 {
        return None;
    }
    let number = |part: &str| -> Option<u64> {
        let digits = !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
        digits.then(|| part.parse().ok()).flatten()
    };
    let (hours, minutes, seconds) = (number(hours)?, number(minutes)?, number(seconds)?);
    if minutes > 59 || seconds > 59 {
        return None;
    }
    Some(Duration::from_secs(hours * 3600 + minutes * 60 + seconds))
}

/// `H:MM:SS`.
fn upnp_time(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!(
        "{}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// The answer to an action the fake doesn't serve: UPnP's 401, Invalid Action.
fn invalid_action() -> Response {
    upnp_fault(401)
}

/// A SOAP fault carrying UPnP error `code`.
fn upnp_fault(code: u16) -> Response {
    xml(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>{code}</errorCode></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#
        ),
    )
}

fn xml(status: StatusCode, body: String) -> Response {
    (
        status,
        [(CONTENT_TYPE, r#"text/xml; charset="utf-8""#)],
        body,
    )
        .into_response()
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            character => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seconds(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    /// Calls AVTransport `action` on `player` with `arguments`, as the Bridge does.
    async fn call(household: &Household, player: &PlayerId, action: &str, arguments: &str) {
        let addr = household.shared.lock().player(player).addr;
        let response = reqwest::Client::new()
            .post(format!("http://{addr}/MediaRenderer/AVTransport/Control"))
            .header("soapaction", format!("\"{AV_TRANSPORT}#{action}\""))
            .body(format!(
                r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action} xmlns:u="{AV_TRANSPORT}"><InstanceID>0</InstanceID>{arguments}</u:{action}></s:Body></s:Envelope>"#
            ))
            .send()
            .await
            .expect("the player answers");
        assert!(
            response.status().is_success(),
            "{action}: {}",
            response.status()
        );
    }

    async fn clock_text(household: &Household, query: &str) -> String {
        let url = format!("http://{}/clock{query}", household.clock_address());
        let response = reqwest::get(url).await.expect("the clock answers");
        response.text().await.expect("a body")
    }

    #[tokio::test]
    async fn a_track_played_to_its_end_stops_back_at_its_start_still_loaded() {
        let (household, clock) = Household::with_manual_clock();
        let kitchen = household.add_player("Kitchen");
        let track = FakeTrack::new("Moby Dick", seconds(600));
        household.play(&kitchen, track.clone());

        clock.advance(seconds(599));
        assert_eq!(household.transport_state(&kitchen), "PLAYING");
        assert_eq!(household.position(&kitchen), seconds(599));

        clock.advance(seconds(5));
        assert_eq!(household.transport_state(&kitchen), "STOPPED");
        assert_eq!(household.position(&kitchen), Duration::ZERO);
        assert_eq!(household.uri(&kitchen), Some(track.uri));
        clock.advance(seconds(60));
        assert_eq!(household.position(&kitchen), Duration::ZERO);
    }

    #[tokio::test]
    async fn a_paused_track_does_not_stop_at_its_end() {
        let (household, clock) = Household::with_manual_clock();
        let kitchen = household.add_player("Kitchen");
        household.play(&kitchen, FakeTrack::new("Moby Dick", seconds(600)));
        clock.advance(seconds(598));
        household.pause(&kitchen);

        clock.advance(seconds(60));
        assert_eq!(household.transport_state(&kitchen), "PAUSED_PLAYBACK");
        assert_eq!(household.position(&kitchen), seconds(598));
    }

    #[tokio::test]
    async fn a_url_plays_without_end_unless_given_a_length() {
        let (household, clock) = Household::with_manual_clock();
        let (kitchen, office) = (
            household.add_player("Kitchen"),
            household.add_player("Office"),
        );
        household.set_url_length(&office, seconds(90));
        for player in [&kitchen, &office] {
            let uri = "<CurrentURI>http://127.0.0.1:9/a.mp3</CurrentURI><CurrentURIMetaData/>";
            call(&household, player, "SetAVTransportURI", uri).await;
            call(&household, player, "Play", "<Speed>1</Speed>").await;
        }

        clock.advance(seconds(100));
        assert_eq!(household.transport_state(&kitchen), "PLAYING");
        assert_eq!(household.position(&kitchen), seconds(100));
        assert_eq!(household.transport_state(&office), "STOPPED");
        assert_eq!(household.position(&office), Duration::ZERO);
    }

    #[tokio::test]
    async fn the_clock_is_served_and_a_follower_waits_for_it_to_move() {
        let (household, clock) = Household::with_manual_clock();
        assert_eq!(clock_text(&household, "").await, "0");
        clock.advance(Duration::from_nanos(1500));
        assert_eq!(clock_text(&household, "").await, "1500");
        // Already past `after`: at once.
        assert_eq!(clock_text(&household, "?after=1000").await, "1500");

        // Caught up: it waits until the clock moves, and is seen waiting meanwhile.
        let (moved, ()) = tokio::join!(clock_text(&household, "?after=1500"), async {
            household.clock_followed().await;
            clock.advance(seconds(30));
        });
        assert_eq!(moved, "30000001500");
    }
}
