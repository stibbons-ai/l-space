//! A fake Sonos household for tests (PRD "Testing Decisions"): players on loopback that answer
//! SSDP M-SEARCH and serve their device description and the read-only UPnP subset the Bridge
//! uses (ZoneGroupTopology `GetZoneGroupState`, AVTransport `GetTransportInfo` and
//! `GetPositionInfo`), with Groups, playback and faults the test controls.
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
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use clock::{Clock, ManualClock};
use tokio::runtime::Runtime;

const ZONE_PLAYER: &str = "urn:schemas-upnp-org:device:ZonePlayer:1";
const TOPOLOGY: &str = "urn:schemas-upnp-org:service:ZoneGroupTopology:1";
const AV_TRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";

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
    pub duration: Duration,
}

impl FakeTrack {
    /// A track titled `title`, without a creator.
    pub fn new(title: &str, duration: Duration) -> Self {
        Self {
            title: Some(title.to_owned()),
            creator: None,
            uri: "http://127.0.0.1:1/track.mp3".to_owned(),
            duration,
        }
    }

    pub fn by(mut self, creator: &str) -> Self {
        self.creator = Some(creator.to_owned());
        self
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
    /// It answers `GetTransportInfo` with a SOAP fault, and everything else correctly.
    FailTransportInfo,
}

/// A fake Sonos household; dropping it stops every player.
#[derive(Debug)]
pub struct Household {
    shared: Arc<Shared>,
    ssdp: SocketAddr,
    runtime: Option<Runtime>,
}

#[derive(Debug)]
struct Shared {
    clock: Arc<dyn Clock>,
    state: Mutex<HouseholdState>,
}

#[derive(Debug, Default)]
struct HouseholdState {
    /// In the order they were added.
    players: Vec<Player>,
    /// Every SOAP action called, as the UUID of the player called and `Service#Action`.
    actions: Vec<(String, String)>,
    /// How many M-SEARCHes for ZonePlayers were received.
    searches: usize,
    added: u32,
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
    fault: Fault,
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
            Some(track) => position.min(track.duration),
            None => Duration::ZERO,
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
        let shared = Arc::new(Shared {
            clock,
            state: Mutex::default(),
        });
        {
            let _runtime = runtime.enter();
            let socket = tokio::net::UdpSocket::from_std(socket).expect("SSDP socket");
            runtime.spawn(answer_searches(Arc::clone(&shared), socket));
        }
        Self {
            shared,
            ssdp,
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
                fault: Fault::None,
            });
            uuid
        };
        let runtime = self.runtime();
        let router = Router::new()
            .route("/xml/device_description.xml", get(description))
            .route("/ZoneGroupTopology/Control", post(control))
            .route("/MediaRenderer/AVTransport/Control", post(control))
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
        let now = self.shared.clock.now();
        let mut state = self.shared.lock();
        let transport = &mut state.coordinator_mut(coordinator).transport;
        transport.offset = transport.position(now);
        transport.since = None;
        transport.state = "PAUSED_PLAYBACK";
    }

    /// `coordinator`'s Group stops; its track stays loaded, back at its start.
    pub fn stop(&self, coordinator: &PlayerId) {
        let mut state = self.shared.lock();
        let transport = &mut state.coordinator_mut(coordinator).transport;
        transport.offset = Duration::ZERO;
        transport.since = None;
        transport.state = "STOPPED";
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

    /// How many M-SEARCHes for ZonePlayers the household has received, counted as each
    /// arrives (before it is answered).
    pub fn searches(&self) -> usize {
        self.shared.lock().searches
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

/// Answers a SOAP call on either service by its `SOAPACTION` header.
async fn control(State(served): State<Served>, headers: HeaderMap) -> Response {
    let soap_action = headers
        .get("soapaction")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim_matches('"');
    let (urn, action) = soap_action.split_once('#').unwrap_or(("", soap_action));
    let service = urn.rsplit(':').nth(1).unwrap_or_default();
    let fault = {
        let mut state = served.shared.lock();
        state
            .actions
            .push((served.uuid.clone(), format!("{service}#{action}")));
        state.find(&served.uuid).map(|player| player.fault)
    };
    match fault {
        Some(Fault::Hang) => std::future::pending().await,
        Some(Fault::Garbage) => return xml(StatusCode::OK, "<s:Envelope><unclosed".to_owned()),
        Some(Fault::SlowPosition(delay)) if action == "GetPositionInfo" => {
            tokio::time::sleep(delay).await;
        }
        Some(Fault::FailTransportInfo) if action == "GetTransportInfo" => return invalid_action(),
        Some(
            Fault::None
            | Fault::Padded(_)
            | Fault::SlowPosition(_)
            | Fault::NoTransportState
            | Fault::FailTransportInfo,
        ) => {}
        // Removed from the household: as good as unplugged.
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
    let now = served.shared.clock.now();
    let state = served.shared.lock();
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
                position_info(transport, now)
            }
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
            ("1", upnp_time(track.duration), didl, track.uri.clone())
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
    xml(
        StatusCode::INTERNAL_SERVER_ERROR,
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>401</errorCode></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#.to_owned(),
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
