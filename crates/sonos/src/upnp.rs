//! [`Sonos`] over local UPnP: players found with SSDP, the Group topology read from any of
//! them (ZoneGroupTopology `GetZoneGroupState`), each Group's playback from its coordinator
//! (AVTransport `GetTransportInfo` and `GetPositionInfo`) and its volume (GroupRenderingControl
//! `GetGroupVolume`), a stream started on a Group by its coordinator (AVTransport
//! `SetAVTransportURI`, then `Play`), and a Group controlled by its coordinator (AVTransport
//! `Pause`, `Play` and `Seek`; GroupRenderingControl `SnapshotGroupVolume`, then
//! `SetGroupVolume`).

mod events;
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use clock::Clock;
use futures::StreamExt;
use futures::future::{BoxFuture, join_all};
use futures::stream::FuturesUnordered;
use reqwest::Client;
use tokio::sync::watch;
use tokio::task::AbortHandle;

use crate::soap::{self, CallError, Service, argument};
use crate::xml::{self, ZoneGroup};
use crate::{
    Error, Group, MAX_VOLUME, Member, Playback, Sonos, Stream, Track, TransportState, ssdp,
};

/// How long after one search the next one starts, on the [`Clock`].
pub const REFRESH: Duration = Duration::from_secs(60);

/// How long a player stays known after it last answered a search, on the [`Clock`]: three
/// searches, so one lost datagram doesn't drop it.
pub const EXPIRY: Duration = Duration::from_secs(3 * 60);

/// How long [`Upnp::groups`] may take in all, waiting for the first search included; shorter
/// than the App's limit for a call to the Bridge.
pub const LISTING_TIMEOUT: Duration = Duration::from_secs(8);

/// How long [`Upnp::play`] may take in all, waiting for the first search included. A start
/// in the Bridge keeps this much of its time budget for it (see the Bridge's `START_TIME`).
pub const PLAY_TIMEOUT: Duration = Duration::from_secs(4);

/// How long each of [`Upnp`]'s calls controlling one Group may take in all (reading its
/// playback, pausing, resuming, seeking, setting its volume), waiting for the first search
/// included.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(4);

/// How long one SOAP call may take, connecting included.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the topology waits for the player asked last before it asks the next one too, if
/// that player has neither answered nor failed: one that stopped answering delays asking the
/// next by this much, not by [`CALL_TIMEOUT`]. The last one asked, and a coordinator asked for
/// its playback, may still take up to [`CALL_TIMEOUT`].
const STAGGER: Duration = Duration::from_millis(500);

/// Sonos over local UPnP. Searching for players starts with [`Upnp::start`] and repeats every
/// [`REFRESH`] until this is dropped.
#[derive(Debug)]
pub struct Upnp {
    shared: Arc<Shared>,
    discovery: AbortHandle,
    event_task: Mutex<Option<AbortHandle>>,
}

#[derive(Debug)]
struct Shared {
    http: Client,
    clock: Arc<dyn Clock>,
    known: Mutex<Known>,
    /// Whether the first search is over (or searching stopped for good).
    searched: watch::Sender<bool>,
    events: events::Events,
    event_clock: Mutex<Arc<dyn Clock>>,
}

/// What the searches and listings found out.
#[derive(Debug, Default)]
struct Known {
    /// The players found, by UUID; at most [`ssdp::MAX_PLAYERS`].
    players: BTreeMap<String, Player>,
    /// Whether the last search could not be sent (its socket not bound, or its datagram not
    /// sent), or searching stopped for good.
    search_failed: bool,
    /// How promising each player is to ask for the topology, from the last one read; see
    /// [`ranks`].
    ranks: BTreeMap<String, Rank>,
}

#[derive(Debug)]
struct Player {
    /// Where its UPnP services answer, as it announced.
    addr: SocketAddr,
    /// When it last answered a search.
    seen: Instant,
}

/// How promising a player is to ask for the topology, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    /// It told the last topology, as one of its visible members.
    LastTeller,
    VisibleCoordinator,
    VisibleMember,
    /// Not a member of the last topology read (or none was read yet). Bonded home theatre
    /// satellites are listed only as `Satellite`s, which aren't members, so they rank here:
    /// after every visible player.
    Unlisted,
    /// Members the Sonos app doesn't show (`Invisible="1"`, such as the second speaker of a
    /// stereo pair or a bonded speaker on older firmware), which can tell an outdated topology
    /// (SoCo, `zonegroupstate.py`).
    Invisible,
}

impl Upnp {
    /// Starts searching for players by sending M-SEARCH to `ssdp` (normally [`crate::MULTICAST`]),
    /// timed by `clock`. Must be called on a Tokio runtime.
    pub fn start(ssdp: SocketAddr, clock: Arc<dyn Clock>) -> Self {
        let shared = Arc::new(Shared::new(clock));
        let discovery = tokio::spawn(discover(Arc::clone(&shared), ssdp));
        let handle = discovery.abort_handle();
        let watched = Arc::clone(&shared);
        tokio::spawn(async move {
            // The search loops forever: it ends only by panicking, or being aborted on drop.
            if let Err(error) = discovery.await
                && !error.is_cancelled()
            {
                tracing::error!(%error, "the search for Sonos players stopped");
                watched.stopped();
            }
        });
        Self {
            shared,
            discovery: handle,
            event_task: Mutex::new(None),
        }
    }

    async fn groups(&self) -> Result<Vec<Group>, Error> {
        let deadline = tokio::time::Instant::now() + LISTING_TIMEOUT;
        self.shared.groups(deadline).await
    }

    async fn play(&self, group: &str, stream: &Stream) -> Result<(), Error> {
        let deadline = tokio::time::Instant::now() + PLAY_TIMEOUT;
        self.shared.play(group, stream, deadline).await.map(drop)
    }

    async fn playback(&self, group: &str) -> Result<Playback, Error> {
        let deadline = tokio::time::Instant::now() + CONTROL_TIMEOUT;
        self.shared.playback(group, deadline).await
    }

    async fn control(&self, group: &str, control: Control) -> Result<(), Error> {
        let deadline = tokio::time::Instant::now() + CONTROL_TIMEOUT;
        self.shared.control(group, control, deadline).await
    }
}

impl Drop for Upnp {
    fn drop(&mut self) {
        self.discovery.abort();
        if let Some(task) = self
            .event_task
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
        {
            task.abort();
        }
    }
}

impl Sonos for Upnp {
    fn topology(&self) -> BoxFuture<'_, Result<Vec<Group>, Error>> {
        Box::pin(async {
            let (players, topology) = self
                .shared
                .topology_by(tokio::time::Instant::now() + CONTROL_TIMEOUT)
                .await?;
            if players.is_empty() {
                return Err(Error::SearchFailed);
            }
            Ok(topology.iter().filter_map(visible).collect())
        })
    }
    fn events(&self, listen: SocketAddr, clock: Arc<dyn Clock>) -> watch::Receiver<u64> {
        let mut task = self
            .event_task
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if task.is_none() {
            *self
                .shared
                .event_clock
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Arc::clone(&clock);
            *task = Some(
                tokio::spawn(events::run(Arc::clone(&self.shared), listen, clock)).abort_handle(),
            );
        }
        self.shared.events.changed.subscribe()
    }

    fn notify(&self, sid: &str, seq: u32, body: &str) -> crate::Notification {
        let now = self
            .shared
            .event_clock
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .now();
        self.shared.events.notify(sid, seq, body, now)
    }

    fn groups(&self) -> BoxFuture<'_, Result<Vec<Group>, Error>> {
        Box::pin(self.groups())
    }

    fn play<'a>(&'a self, group: &'a str, stream: &'a Stream) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(self.play(group, stream))
    }

    fn playback<'a>(&'a self, group: &'a str) -> BoxFuture<'a, Result<Playback, Error>> {
        Box::pin(self.playback(group))
    }

    fn pause<'a>(&'a self, group: &'a str) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(self.control(group, Control::Pause))
    }

    fn resume<'a>(&'a self, group: &'a str) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(self.control(group, Control::Resume))
    }

    fn seek<'a>(&'a self, group: &'a str, position: Duration) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(self.control(group, Control::Seek(position)))
    }

    fn set_volume<'a>(&'a self, group: &'a str, volume: u8) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(self.control(group, Control::SetVolume(volume.min(MAX_VOLUME))))
    }
}

/// An action that controls a Group, on its coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Pause,
    Resume,
    /// To this far into the current track.
    Seek(Duration),
    /// To this volume, at most [`MAX_VOLUME`].
    SetVolume(u8),
}

impl Control {
    /// The service, action and input arguments (name, unescaped value) that do it.
    fn call(self) -> (Service, &'static str, Vec<(&'static str, String)>) {
        match self {
            Self::Pause => (Service::AvTransport, "Pause", Vec::new()),
            Self::Resume => (
                Service::AvTransport,
                "Play",
                vec![("Speed", "1".to_owned())],
            ),
            Self::Seek(position) => (
                Service::AvTransport,
                "Seek",
                vec![
                    ("Unit", "REL_TIME".to_owned()),
                    ("Target", rel_time(position)),
                ],
            ),
            Self::SetVolume(volume) => (
                Service::GroupRenderingControl,
                "SetGroupVolume",
                vec![("DesiredVolume", volume.to_string())],
            ),
        }
    }
}

/// Searches for players now and then every [`REFRESH`], forever.
async fn discover(shared: Arc<Shared>, target: SocketAddr) {
    loop {
        let found = ssdp::search(target).await.map_err(|error| {
            tracing::warn!(%target, %error, "SSDP search failed");
        });
        shared.saw(found);
        shared.clock.sleep(REFRESH).await;
    }
}

impl Shared {
    fn new(clock: Arc<dyn Clock>) -> Self {
        let http = Client::builder()
            .timeout(CALL_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .expect("an HTTP client without TLS builds");
        Self {
            event_clock: Mutex::new(Arc::clone(&clock)),
            http,
            clock,
            known: Mutex::default(),
            searched: watch::Sender::new(false),
            events: events::Events::default(),
        }
    }

    fn known(&self) -> MutexGuard<'_, Known> {
        self.known.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records what a search found (or that it failed): forgets the players unseen for
    /// [`EXPIRY`], keeps those found (at most [`ssdp::MAX_PLAYERS`]), and marks the first
    /// search over.
    fn saw(&self, found: Result<Vec<ssdp::Found>, ()>) {
        let now = self.clock.now();
        let mut known = self.known();
        known.search_failed = found.is_err();
        let players = &mut known.players;
        let before = players.len();
        players.retain(|_, player| now.duration_since(player.seen) < EXPIRY);
        let mut changed = players.len() < before;
        let mut refused = 0;
        for player in found.unwrap_or_default() {
            if let Some(known) = players.get_mut(&player.uuid) {
                *known = Player {
                    addr: player.addr,
                    seen: now,
                };
            } else if players.len() < ssdp::MAX_PLAYERS {
                let addr = player.addr;
                players.insert(player.uuid, Player { addr, seen: now });
                changed = true;
            } else {
                refused += 1;
            }
        }
        if refused > 0 {
            tracing::warn!(
                refused,
                players = ssdp::MAX_PLAYERS,
                "Sonos players not kept: the most players are already known"
            );
        }
        if changed {
            let uuids: Vec<&String> = players.keys().collect();
            tracing::info!(players = ?uuids, "Sonos players changed");
        }
        drop(known);
        self.searched.send_replace(true);
    }

    /// Searching stopped for good: listings stop waiting for a first search, and with no
    /// player known they say the Bridge can't search.
    fn stopped(&self) {
        self.known().search_failed = true;
        self.searched.send_replace(true);
    }

    /// Every Group, if the topology could be read by `deadline`. A Group whose coordinator
    /// didn't tell its playback by then is listed without what it didn't tell.
    async fn groups(&self, deadline: tokio::time::Instant) -> Result<Vec<Group>, Error> {
        let (players, topology) = self.topology_by(deadline).await?;
        let mut groups = join_all(topology.iter().filter_map(visible).map(|group| {
            // Only an address a player announced for itself: the topology's could point anywhere.
            let coordinator = players.get(&group.id).copied();
            self.with_playback(group, coordinator, deadline)
        }))
        .await;
        groups.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(groups)
    }

    /// The players known now (by UUID) and the Group topology, read by `deadline`; no
    /// topology when a working search found no player.
    async fn topology_by(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<(BTreeMap<String, SocketAddr>, Vec<ZoneGroup>), Error> {
        // Without a first search, an empty list would only mean "not searched yet".
        let mut searched = self.searched.subscribe();
        if tokio::time::timeout_at(deadline, searched.wait_for(|searched| *searched))
            .await
            .is_err()
        {
            tracing::warn!("the first search for Sonos players did not end in time");
            return Err(Error::SearchFailed);
        }
        let (players, candidates) = {
            let known = self.known();
            // Expired players are skipped here too, in case searching stopped.
            let now = self.clock.now();
            let players: BTreeMap<String, SocketAddr> = known
                .players
                .iter()
                .filter(|(_, player)| now.duration_since(player.seen) < EXPIRY)
                .map(|(uuid, player)| (uuid.clone(), player.addr))
                .collect();
            if players.is_empty() {
                return if known.search_failed {
                    Err(Error::SearchFailed)
                } else {
                    Ok((players, Vec::new()))
                };
            }
            let candidates = candidates(&players, &known.ranks);
            (players, candidates)
        };
        let Ok(topology) = tokio::time::timeout_at(deadline, self.topology(&candidates)).await
        else {
            tracing::warn!("no Sonos player told the Group topology in time");
            return Err(Error::Unreachable);
        };
        let (from, topology) = topology?;
        self.known().ranks = ranks(&from, &topology);
        Ok((players, topology))
    }

    /// The address of Group `group`'s coordinator, as the topology read by `deadline` names it
    /// and as the coordinator announced itself to the searches.
    async fn coordinator(
        &self,
        group: &str,
        deadline: tokio::time::Instant,
    ) -> Result<SocketAddr, Error> {
        let (players, topology) = self.topology_by(deadline).await?;
        let known = topology
            .iter()
            .any(|zone| zone.coordinator == group && visible(zone).is_some());
        if !known {
            return Err(Error::UnknownGroup);
        }
        // Only an address a player announced for itself: the topology's could point anywhere.
        players.get(group).copied().ok_or_else(|| {
            tracing::warn!(
                group,
                "a Group's coordinator did not answer the last searches"
            );
            Error::GroupFailed
        })
    }

    /// Calls `action` on `service` of Group `group`'s coordinator at `coordinator` with the
    /// input `arguments`, by `deadline`. A failure is logged as the coordinator not doing
    /// `what`.
    async fn call_coordinator(
        &self,
        (group, coordinator): (&str, SocketAddr),
        (service, action): (Service, &str),
        arguments: &[(&str, &str)],
        deadline: tokio::time::Instant,
        what: &str,
    ) -> Result<Vec<(String, String)>, Error> {
        let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
        soap::call_with(&self.http, coordinator, service, action, arguments, timeout)
            .await
            .map_err(|error| {
                tracing::warn!(group, action, %error, what, "a Group's coordinator failed");
                Error::GroupFailed
            })
    }

    /// Has Group `group` play `stream` from its start, by `deadline`: its coordinator, as the
    /// topology read now names it, is given the stream's URL (`SetAVTransportURI`, without
    /// metadata) and told to play. Returns the URL.
    async fn play(
        &self,
        group: &str,
        stream: &Stream,
        deadline: tokio::time::Instant,
    ) -> Result<String, Error> {
        let coordinator = self.coordinator(group, deadline).await?;
        let host = match route_to(coordinator.ip()) {
            Ok(host) => host,
            Err(error) => {
                let peer = coordinator.ip();
                tracing::warn!(group, %peer, %error, "no route to a Group's coordinator");
                return Err(Error::GroupFailed);
            }
        };
        let url = stream.url(host);
        let call = async |action, arguments: &[(&str, &str)]| {
            let at = (group, coordinator);
            let what = "start playing";
            self.call_coordinator(
                at,
                (Service::AvTransport, action),
                arguments,
                deadline,
                what,
            )
            .await
        };
        call(
            "SetAVTransportURI",
            &[("CurrentURI", &url), ("CurrentURIMetaData", "")],
        )
        .await?;
        call("Play", &[("Speed", "1")]).await?;
        Ok(url)
    }

    /// What Group `group` plays, as its coordinator tells it by `deadline`: its transport
    /// state, and its track's URL, position and length. A coordinator that doesn't tell
    /// both, or tells no transport state at all, failed.
    async fn playback(
        &self,
        group: &str,
        deadline: tokio::time::Instant,
    ) -> Result<Playback, Error> {
        let coordinator = self.coordinator(group, deadline).await?;
        let call = |action| {
            let at = (group, coordinator);
            let what = "tell its playback";
            self.call_coordinator(at, (Service::AvTransport, action), &[], deadline, what)
        };
        let (transport, position) = tokio::join!(call("GetTransportInfo"), call("GetPositionInfo"));
        let (transport, position) = (transport?, position?);
        if argument(&transport, "CurrentTransportState").is_none() {
            tracing::warn!(group, "a Group's coordinator told no transport state");
            return Err(Error::GroupFailed);
        }
        let uri = argument(&position, "TrackURI")
            .map(str::trim)
            .filter(|uri| !uri.is_empty());
        Ok(Playback {
            state: state(group, &transport),
            uri: uri.map(str::to_owned),
            position: uri
                .and(argument(&position, "RelTime"))
                .and_then(xml::duration),
            duration: track_duration(&position),
        })
    }

    /// Has Group `group`'s coordinator do `control`, by `deadline`. A volume is set right
    /// after a fresh `SnapshotGroupVolume`: `SetGroupVolume` changes the members' volumes in
    /// proportion to the last snapshot, which may predate a member's volume changed on its own.
    async fn control(
        &self,
        group: &str,
        control: Control,
        deadline: tokio::time::Instant,
    ) -> Result<(), Error> {
        let coordinator = self.coordinator(group, deadline).await?;
        let at = (group, coordinator);
        let what = "control the Group";
        if let Control::SetVolume(_) = control {
            let snapshot = (Service::GroupRenderingControl, "SnapshotGroupVolume");
            self.call_coordinator(at, snapshot, &[], deadline, what)
                .await?;
        }
        let (service, action, arguments) = control.call();
        let arguments: Vec<(&str, &str)> = arguments
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        self.call_coordinator(at, (service, action), &arguments, deadline, what)
            .await
            .map(drop)
    }

    /// The Group topology and the UUID of the player that told it, asking `candidates` (UUID,
    /// address) in order. Every player knows the whole household's. The next player is asked
    /// as soon as one fails, or after [`STAGGER`] if the one asked last hasn't answered yet.
    async fn topology(
        &self,
        candidates: &[(String, SocketAddr)],
    ) -> Result<(String, Vec<ZoneGroup>), Error> {
        let mut asked = FuturesUnordered::new();
        let mut next = candidates.iter();
        loop {
            if let Some((uuid, player)) = next.next() {
                asked.push(async move { (uuid, *player, self.zone_groups(*player).await) });
            }
            if asked.is_empty() {
                return Err(Error::Unreachable);
            }
            let more = next.len() > 0;
            tokio::select! {
                Some((uuid, player, answer)) = asked.next() => match answer {
                    Ok(topology) => return Ok((uuid.clone(), topology)),
                    Err(error) => tracing::warn!(%player, %error, "cannot read the Group topology"),
                },
                () = tokio::time::sleep(STAGGER), if more => {}
            }
        }
    }

    async fn zone_groups(&self, player: SocketAddr) -> Result<Vec<ZoneGroup>, CallError> {
        let answer = soap::call(
            &self.http,
            player,
            Service::ZoneGroupTopology,
            "GetZoneGroupState",
        )
        .await?;
        let state =
            argument(&answer, "ZoneGroupState").ok_or(xml::Malformed::Missing("ZoneGroupState"))?;
        Ok(xml::zone_groups(state)?)
    }

    /// `group` with its transport state, track and volume, as its coordinator (at
    /// `coordinator`, if found by a search) tells them by `deadline`. Each is kept if it came
    /// in time; a track without a state is not shown.
    async fn with_playback(
        &self,
        mut group: Group,
        coordinator: Option<SocketAddr>,
        deadline: tokio::time::Instant,
    ) -> Group {
        let Some(coordinator) = coordinator else {
            tracing::warn!(
                group = group.id,
                "a Group's coordinator did not answer the last searches"
            );
            return group;
        };
        let call = |service, action| async move {
            tokio::time::timeout_at(
                deadline,
                soap::call(&self.http, coordinator, service, action),
            )
            .await
        };
        let (transport, position, volume) = tokio::join!(
            call(Service::AvTransport, "GetTransportInfo"),
            call(Service::AvTransport, "GetPositionInfo"),
            call(Service::GroupRenderingControl, "GetGroupVolume"),
        );
        match transport {
            Ok(Ok(answer)) => group.state = Some(state(&group.id, &answer)),
            Ok(Err(error)) => {
                tracing::warn!(group = group.id, %error, "cannot read a Group's transport state")
            }
            Err(_) => tracing::warn!(
                group = group.id,
                "a Group's coordinator did not tell its transport state within the listing's time"
            ),
        }
        match position {
            Ok(Ok(answer)) if group.state.is_some() => group.track = track(&answer),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::warn!(group = group.id, %error, "cannot read a Group's track")
            }
            Err(_) => tracing::warn!(
                group = group.id,
                "a Group's coordinator did not tell its track within the listing's time"
            ),
        }
        match volume {
            Ok(Ok(answer)) => group.volume = group_volume(&group.id, &answer),
            Ok(Err(error)) => {
                tracing::warn!(group = group.id, %error, "cannot read a Group's volume")
            }
            Err(_) => tracing::warn!(
                group = group.id,
                "a Group's coordinator did not tell its volume within the listing's time"
            ),
        }
        group
    }
}

/// This host's address on the route to `peer`: where `peer` reaches it. Connecting a UDP
/// socket sends nothing; it only picks the route.
fn route_to(peer: IpAddr) -> std::io::Result<IpAddr> {
    let unspecified: IpAddr = match peer {
        IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    };
    let socket = UdpSocket::bind((unspecified, 0))?;
    // Any port: nothing is sent.
    socket.connect((peer, 9))?;
    let local = socket.local_addr()?.ip();
    if local.is_unspecified() {
        return Err(std::io::Error::other("the route has no local address"));
    }
    Ok(local)
}

/// The transport state in Group `group`'s `GetTransportInfo` answer: unknown, and logged with
/// its first 32 characters, when it is missing or not one the Bridge knows.
fn state(group: &str, answer: &[(String, String)]) -> TransportState {
    let raw = argument(answer, "CurrentTransportState");
    let state = raw.map_or(TransportState::Unknown, xml::transport_state);
    if state == TransportState::Unknown {
        let state: String = raw.unwrap_or_default().chars().take(32).collect();
        tracing::warn!(group, state, "unknown Sonos transport state");
    }
    state
}

/// `players` (by UUID) in the order to ask them for the topology: by their [`Rank`] from the
/// last topology read, ties in UUID order.
fn candidates(
    players: &BTreeMap<String, SocketAddr>,
    ranks: &BTreeMap<String, Rank>,
) -> Vec<(String, SocketAddr)> {
    let mut candidates: Vec<(String, SocketAddr)> = players
        .iter()
        .map(|(uuid, addr)| (uuid.clone(), *addr))
        .collect();
    candidates.sort_by_key(|(uuid, _)| ranks.get(uuid).copied().unwrap_or(Rank::Unlisted));
    candidates
}

/// How promising each player in `topology` is to ask for it next time. The player that told
/// it (`from`) comes first only if it is a visible member; otherwise it keeps the rank of
/// what it is (an invisible member, or unlisted).
fn ranks(from: &str, topology: &[ZoneGroup]) -> BTreeMap<String, Rank> {
    let mut ranks = BTreeMap::new();
    for zone in topology {
        for member in &zone.members {
            let rank = match (member.visible, member.uuid == zone.coordinator) {
                (true, true) => Rank::VisibleCoordinator,
                (true, false) => Rank::VisibleMember,
                (false, _) => Rank::Invisible,
            };
            ranks.insert(member.uuid.clone(), rank);
        }
    }
    if let Some(rank) = ranks.get_mut(from)
        && matches!(rank, Rank::VisibleCoordinator | Rank::VisibleMember)
    {
        *rank = Rank::LastTeller;
    }
    ranks
}

/// The Group the Sonos app shows for `zone`, without its playback; none if no member is
/// visible.
fn visible(zone: &ZoneGroup) -> Option<Group> {
    let mut members: Vec<Member> = zone
        .members
        .iter()
        .filter(|member| member.visible)
        .map(|member| Member {
            id: member.uuid.clone(),
            name: member.name.clone(),
        })
        .collect();
    if members.is_empty() {
        return None;
    }
    // Stable: the coordinator first, the others in the topology's order.
    members.sort_by_key(|member| member.id != zone.coordinator);
    // Bonded speakers share their name, so an invisible coordinator still names the Group.
    let name = zone
        .members
        .iter()
        .find(|member| member.uuid == zone.coordinator)
        .map(|coordinator| coordinator.name.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| members[0].name.clone());
    Some(Group {
        id: zone.coordinator.clone(),
        name,
        members,
        state: None,
        track: None,
        volume: None,
    })
}

/// The loaded track in a `GetPositionInfo` answer; none without a `TrackURI`.
fn track(answer: &[(String, String)]) -> Option<Track> {
    argument(answer, "TrackURI").filter(|uri| !uri.trim().is_empty())?;
    let (title, creator) =
        xml::title_and_creator(argument(answer, "TrackMetaData").unwrap_or_default());
    Some(Track {
        title,
        creator,
        position: argument(answer, "RelTime").and_then(xml::duration),
        duration: track_duration(answer),
    })
}

/// The track's length in a `GetPositionInfo` answer; none when unknown, which Sonos tells as
/// `0:00:00` (as for a stream).
fn track_duration(answer: &[(String, String)]) -> Option<Duration> {
    argument(answer, "TrackDuration")
        .and_then(xml::duration)
        .filter(|duration| !duration.is_zero())
}

/// The volume in Group `group`'s `GetGroupVolume` answer: none, and logged, when it is
/// missing, not a number, or above [`MAX_VOLUME`].
fn group_volume(group: &str, answer: &[(String, String)]) -> Option<u8> {
    let raw = argument(answer, "CurrentVolume");
    let volume = raw
        .and_then(|volume| volume.trim().parse::<u8>().ok())
        .filter(|volume| *volume <= MAX_VOLUME);
    if volume.is_none() {
        let volume: String = raw.unwrap_or_default().chars().take(32).collect();
        tracing::warn!(group, volume, "a Group's volume is not one Sonos uses");
    }
    volume
}

/// `position` as AVTransport's `REL_TIME` target: `H:MM:SS`, whole seconds, any fraction
/// dropped.
fn rel_time(position: Duration) -> String {
    let seconds = position.as_secs();
    format!(
        "{}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use clock::ManualClock;
    use sonos_fake::{FakeTrack, Fault, Household, PlayerId};

    use super::*;
    use crate::xml::ZoneMember;

    /// The SOAP actions that give a Group a URL and have it play, as `sonos-fake` records them.
    const SET_URI: &str = "AVTransport#SetAVTransportURI";
    const PLAY: &str = "AVTransport#Play";

    fn member(uuid: &str, name: &str, visible: bool) -> ZoneMember {
        ZoneMember {
            uuid: uuid.into(),
            name: name.into(),
            visible,
        }
    }

    fn zone(coordinator: &str, members: Vec<ZoneMember>) -> ZoneGroup {
        ZoneGroup {
            coordinator: coordinator.into(),
            members,
        }
    }

    fn ids(group: &Group) -> Vec<&str> {
        group
            .members
            .iter()
            .map(|member| member.id.as_str())
            .collect()
    }

    #[test]
    fn a_group_without_a_visible_member_is_not_shown() {
        let boost = zone("RINCON_BOOST", vec![member("RINCON_BOOST", "BOOST", false)]);
        assert_eq!(visible(&boost), None);
        assert_eq!(visible(&zone("RINCON_X", Vec::new())), None);
    }

    #[test]
    fn the_coordinator_comes_first_among_the_members() {
        let kitchen = zone(
            "RINCON_K",
            vec![
                member("RINCON_B", "Bath", true),
                member("RINCON_O", "Office", true),
                member("RINCON_K", "Kitchen", true),
            ],
        );
        let group = visible(&kitchen).unwrap();
        assert_eq!(
            (group.id.as_str(), group.name.as_str()),
            ("RINCON_K", "Kitchen")
        );
        assert_eq!(ids(&group), ["RINCON_K", "RINCON_B", "RINCON_O"]);
    }

    #[test]
    fn an_invisible_coordinator_names_its_group_without_being_a_member() {
        let pair = zone(
            "RINCON_R",
            vec![
                member("RINCON_L", "Left", true),
                member("RINCON_R", "Living Room", false),
            ],
        );
        let group = visible(&pair).unwrap();
        assert_eq!(
            (group.id.as_str(), group.name.as_str()),
            ("RINCON_R", "Living Room")
        );
        assert_eq!(ids(&group), ["RINCON_L"]);

        // Without a name of its own, the first member's names it.
        let nameless = zone(
            "RINCON_R",
            vec![
                member("RINCON_L", "Den", true),
                member("RINCON_R", "", false),
            ],
        );
        assert_eq!(visible(&nameless).unwrap().name, "Den");
    }

    /// A Kitchen + Bath Group and a TV with a bonded satellite.
    fn household_topology() -> [ZoneGroup; 2] {
        [
            zone(
                "RINCON_K",
                vec![
                    member("RINCON_K", "Kitchen", true),
                    member("RINCON_B", "Bath", true),
                ],
            ),
            zone(
                "RINCON_T",
                vec![
                    member("RINCON_T", "TV", true),
                    member("RINCON_S", "TV", false),
                ],
            ),
        ]
    }

    /// The order players are asked in, after `from` told `topology`.
    fn order(from: &str, topology: &[ZoneGroup], unlisted: &[&str]) -> Vec<String> {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut players: BTreeMap<String, SocketAddr> = topology
            .iter()
            .flat_map(|zone| {
                zone.members
                    .iter()
                    .map(|member| (member.uuid.clone(), addr))
            })
            .collect();
        players.extend(unlisted.iter().map(|uuid| (uuid.to_string(), addr)));
        candidates(&players, &ranks(from, topology))
            .into_iter()
            .map(|(uuid, _)| uuid)
            .collect()
    }

    #[test]
    fn a_visible_teller_is_asked_first_and_satellites_last() {
        assert_eq!(
            order("RINCON_B", &household_topology(), &["RINCON_N"]),
            ["RINCON_B", "RINCON_K", "RINCON_T", "RINCON_N", "RINCON_S"]
        );
    }

    #[test]
    fn a_teller_that_is_invisible_or_unlisted_is_not_promoted() {
        assert_eq!(
            order("RINCON_S", &household_topology(), &["RINCON_N"]),
            ["RINCON_K", "RINCON_T", "RINCON_B", "RINCON_N", "RINCON_S"]
        );
        assert_eq!(
            order("RINCON_N", &household_topology(), &["RINCON_N"]),
            ["RINCON_K", "RINCON_T", "RINCON_B", "RINCON_N", "RINCON_S"]
        );
    }

    /// A [`Shared`] that knows the players of `household`, found by a real search.
    async fn knowing(household: &Household, clock: &ManualClock) -> Shared {
        let shared = Shared::new(Arc::new(clock.clone()));
        shared.saw(Ok(ssdp::search(household.ssdp()).await.unwrap()));
        shared
    }

    fn soon(after: Duration) -> tokio::time::Instant {
        tokio::time::Instant::now() + after
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn what_a_coordinator_tells_in_time_is_kept_and_the_rest_left_out() {
        let (household, clock) = Household::with_manual_clock();
        let kitchen = household.add_player("Kitchen");
        household.play(
            &kitchen,
            FakeTrack::new("Moby Dick", Duration::from_secs(600)),
        );
        // Its track comes after the listing's time, though within one call's.
        household.set_fault(&kitchen, Fault::SlowPosition(Duration::from_millis(1500)));
        let office = household.add_player("Office");
        household.play(&office, FakeTrack::new("Silence", Duration::from_secs(600)));
        let shared = knowing(&household, &clock).await;

        let start = tokio::time::Instant::now();
        let groups = shared
            .groups(soon(Duration::from_millis(800)))
            .await
            .unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(1200),
            "{:?}",
            start.elapsed()
        );
        let names: Vec<&str> = groups.iter().map(|group| group.name.as_str()).collect();
        assert_eq!(names, ["Kitchen", "Office"]);
        assert_eq!(groups[0].state, Some(TransportState::Playing));
        assert_eq!(groups[0].track, None);
        assert_eq!(groups[1].state, Some(TransportState::Playing));
        assert!(groups[1].track.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_search_keeps_the_players_known_and_a_good_one_clears_it() {
        let (household, clock) = Household::with_manual_clock();
        household.add_player("Kitchen");
        let shared = knowing(&household, &clock).await;

        shared.saw(Err(()));
        let groups = shared.groups(soon(LISTING_TIMEOUT)).await.unwrap();
        assert_eq!(groups.len(), 1);

        let shared = Shared::new(Arc::new(clock.clone()));
        shared.saw(Err(()));
        assert_eq!(
            shared.groups(soon(LISTING_TIMEOUT)).await,
            Err(Error::SearchFailed)
        );
        shared.saw(Ok(Vec::new()));
        assert_eq!(shared.groups(soon(LISTING_TIMEOUT)).await, Ok(Vec::new()));
    }

    #[tokio::test]
    async fn never_having_searched_is_a_failed_search() {
        let shared = Shared::new(Arc::new(ManualClock::new()));
        let start = tokio::time::Instant::now();
        assert_eq!(
            shared.groups(soon(Duration::from_millis(50))).await,
            Err(Error::SearchFailed)
        );
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "{:?}",
            start.elapsed()
        );
        shared.stopped();
        let start = tokio::time::Instant::now();
        assert_eq!(
            shared.groups(soon(LISTING_TIMEOUT)).await,
            Err(Error::SearchFailed)
        );
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "{:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn a_listing_waiting_for_the_first_search_ends_when_searching_stops() {
        let shared = Shared::new(Arc::new(ManualClock::new()));
        let start = tokio::time::Instant::now();
        let (listed, ()) = tokio::join!(shared.groups(soon(LISTING_TIMEOUT)), async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            shared.stopped();
        });
        assert_eq!(listed, Err(Error::SearchFailed));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
    }

    fn found(uuid: String) -> ssdp::Found {
        ssdp::Found {
            uuid,
            addr: "127.0.0.1:1".parse().unwrap(),
        }
    }

    #[test]
    fn players_past_the_most_kept_are_refused_until_others_expire() {
        let clock = ManualClock::new();
        let shared = Shared::new(Arc::new(clock.clone()));
        let known = |shared: &Shared| shared.known().players.len();
        let many: Vec<_> = (0..ssdp::MAX_PLAYERS)
            .map(|n| found(format!("RINCON_A{n}")))
            .collect();
        shared.saw(Ok(many));
        assert_eq!(known(&shared), ssdp::MAX_PLAYERS);

        // One more is refused while the others are fresh...
        shared.saw(Ok(vec![found("RINCON_B".into())]));
        assert_eq!(known(&shared), ssdp::MAX_PLAYERS);
        assert!(!shared.known().players.contains_key("RINCON_B"));

        // ...and kept once they have expired, which the same search finds out first.
        clock.advance(EXPIRY);
        shared.saw(Ok(vec![found("RINCON_C".into())]));
        let players: Vec<String> = shared.known().players.keys().cloned().collect();
        assert_eq!(players, ["RINCON_C"]);
    }

    #[test]
    fn a_transport_answer_without_a_known_state_is_an_unknown_state() {
        let without: Vec<(String, String)> = vec![("CurrentTransportStatus".into(), "OK".into())];
        assert_eq!(state("RINCON_K", &without), TransportState::Unknown);
        let recording = vec![("CurrentTransportState".into(), "RECORDING".into())];
        assert_eq!(state("RINCON_K", &recording), TransportState::Unknown);
        let playing = vec![("CurrentTransportState".into(), "PLAYING".into())];
        assert_eq!(state("RINCON_K", &playing), TransportState::Playing);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_state_missing_from_the_answer_is_unknown_and_a_failed_one_shows_no_track() {
        let (household, clock) = Household::with_manual_clock();
        let track = || FakeTrack::new("Moby Dick", Duration::from_secs(600));
        let kitchen = household.add_player("Kitchen");
        household.play(&kitchen, track());
        household.set_fault(&kitchen, Fault::NoTransportState);
        let office = household.add_player("Office");
        household.play(&office, track());
        household.set_fault(&office, Fault::FailTransportInfo);
        let shared = knowing(&household, &clock).await;

        let groups = shared.groups(soon(LISTING_TIMEOUT)).await.unwrap();
        let names: Vec<&str> = groups.iter().map(|group| group.name.as_str()).collect();
        assert_eq!(names, ["Kitchen", "Office"]);
        assert_eq!(groups[0].state, Some(TransportState::Unknown));
        assert!(groups[0].track.is_some());
        // Office told its track, but without a state it isn't shown.
        assert_eq!((groups[1].state, &groups[1].track), (None, &None));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_topology_is_given_up_on_at_the_deadline() {
        let (household, clock) = Household::with_manual_clock();
        for n in 0..6 {
            let silent = household.add_player(&format!("Silent {n}"));
            household.set_fault(&silent, Fault::Hang);
        }
        let shared = knowing(&household, &clock).await;

        // Without the deadline it would take about 4 s: the sixth player is asked at about 2 s,
        // when the first one times out, and times out itself 2 s later.
        let start = tokio::time::Instant::now();
        assert_eq!(
            shared.groups(soon(Duration::from_secs(1))).await,
            Err(Error::Unreachable)
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(950) && elapsed < Duration::from_millis(1300),
            "{elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn players_expire_when_listed_even_after_searching_stopped() {
        let (household, clock) = Household::with_manual_clock();
        household.add_player("Kitchen");
        let shared = knowing(&household, &clock).await;
        shared.stopped();
        assert_eq!(shared.groups(soon(LISTING_TIMEOUT)).await.unwrap().len(), 1);

        clock.advance(EXPIRY);
        assert_eq!(
            shared.groups(soon(LISTING_TIMEOUT)).await,
            Err(Error::SearchFailed)
        );
    }

    fn stream(listen: &str) -> Stream {
        Stream {
            listen: listen.parse().unwrap(),
            path: "/stream/abc/Episode%201.mp3".into(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_group_plays_the_stream_on_the_address_its_coordinator_reaches() {
        let (household, clock) = Household::with_manual_clock();
        let kitchen = household.add_player("Kitchen");
        let bath = household.add_player("Bath");
        household.join(&bath, &kitchen);
        let office = household.add_player("Office");
        let shared = knowing(&household, &clock).await;

        // Listening on every address, the coordinator gets the one on its route: loopback.
        let url = shared
            .play(kitchen.uuid(), &stream("0.0.0.0:9"), soon(PLAY_TIMEOUT))
            .await
            .unwrap();
        assert_eq!(url, "http://127.0.0.1:9/stream/abc/Episode%201.mp3");
        assert_eq!(household.uri(&kitchen).as_deref(), Some(url.as_str()));
        assert_eq!(household.transport_state(&kitchen), "PLAYING");
        let started = [SET_URI, PLAY];
        let calls = household.actions_of(&kitchen);
        assert!(calls.ends_with(&started.map(String::from)), "{calls:?}");
        assert!(
            !household
                .actions_of(&bath)
                .iter()
                .any(|a| a.contains("Play"))
        );
        assert!(
            !household
                .actions_of(&office)
                .iter()
                .any(|a| a.contains("Play"))
        );
        assert_eq!(household.uri(&office), None);

        // Listening on one address, that one.
        let url = shared
            .play(office.uuid(), &stream("127.0.0.2:9"), soon(PLAY_TIMEOUT))
            .await
            .unwrap();
        assert_eq!(url, "http://127.0.0.2:9/stream/abc/Episode%201.mp3");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_a_group_in_the_topology_plays() {
        let (household, clock) = Household::with_manual_clock();
        let kitchen = household.add_player("Kitchen");
        let bath = household.add_player("Bath");
        household.join(&bath, &kitchen);
        let beam = household.add_player("TV");
        let surround = household.add_bonded(&beam);
        let shared = knowing(&household, &clock).await;

        // A member that doesn't coordinate, a bonded speaker, and no player at all.
        for group in [bath.uuid(), surround.uuid(), "RINCON_NOSUCH01400"] {
            assert_eq!(
                shared
                    .play(group, &stream("0.0.0.0:9"), soon(PLAY_TIMEOUT))
                    .await,
                Err(Error::UnknownGroup),
                "{group}"
            );
        }
        assert!(
            !household
                .actions()
                .iter()
                .any(|a| a.contains("SetAVTransportURI"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_coordinator_that_refuses_or_hangs_fails_in_time() {
        let (household, clock) = Household::with_manual_clock();
        let kitchen = household.add_player("Kitchen");
        household.set_fault(&kitchen, Fault::RefusePlayback);
        let office = household.add_player("Office");
        household.set_fault(&office, Fault::Hang);
        let shared = knowing(&household, &clock).await;

        assert_eq!(
            shared
                .play(kitchen.uuid(), &stream("0.0.0.0:9"), soon(PLAY_TIMEOUT))
                .await,
            Err(Error::GroupFailed)
        );
        assert_eq!(household.transport_state(&kitchen), "STOPPED");
        // It failed at the coordinator's refusal, not before.
        let set_uri = SET_URI.to_owned();
        assert!(household.actions_of(&kitchen).contains(&set_uri));

        // The topology comes from Kitchen; Office never answers its own calls.
        let start = tokio::time::Instant::now();
        assert_eq!(
            shared
                .play(
                    office.uuid(),
                    &stream("0.0.0.0:9"),
                    soon(Duration::from_secs(1))
                )
                .await,
            Err(Error::GroupFailed)
        );
        let elapsed = start.elapsed();
        // It waited for Office up to the deadline, then gave up.
        assert!(
            elapsed >= Duration::from_millis(900) && elapsed < Duration::from_secs(2),
            "{elapsed:?}"
        );
        assert!(household.actions_of(&office).contains(&set_uri));
    }

    #[test]
    fn a_seek_target_is_hours_minutes_and_seconds_without_a_fraction() {
        for (seconds, millis, expected) in [
            (0, 0, "0:00:00"),
            (59, 999, "0:00:59"),
            (61, 0, "0:01:01"),
            (3600, 0, "1:00:00"),
            (3661, 500, "1:01:01"),
            (36_000 + 59 * 60 + 59, 0, "10:59:59"),
        ] {
            let position = Duration::from_secs(seconds) + Duration::from_millis(millis);
            assert_eq!(rel_time(position), expected, "{position:?}");
        }
    }

    #[test]
    fn a_group_volume_is_a_number_up_to_the_highest() {
        let answer = |volume: &str| vec![("CurrentVolume".to_owned(), volume.to_owned())];
        assert_eq!(group_volume("RINCON_K", &answer("0")), Some(0));
        assert_eq!(group_volume("RINCON_K", &answer(" 35 ")), Some(35));
        assert_eq!(group_volume("RINCON_K", &answer("100")), Some(MAX_VOLUME));
        for unusable in ["101", "-1", "", "loud", "35.5"] {
            assert_eq!(
                group_volume("RINCON_K", &answer(unusable)),
                None,
                "{unusable:?}"
            );
        }
        assert_eq!(group_volume("RINCON_K", &[]), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_listing_reads_each_groups_volume_when_told() {
        let (household, clock) = Household::with_manual_clock();
        let kitchen = household.add_player("Kitchen");
        household.set_volume(&kitchen, 35);
        let office = household.add_player("Office");
        household.set_fault(&office, Fault::FailGroupVolume);
        let shared = knowing(&household, &clock).await;

        let groups = shared.groups(soon(LISTING_TIMEOUT)).await.unwrap();
        let volumes: Vec<_> = groups.iter().map(|group| group.volume).collect();
        assert_eq!(volumes, [Some(35), None]);
        // Office's state still came, without its volume.
        assert_eq!(groups[1].state, Some(TransportState::Stopped));
    }

    /// The SOAP actions that control a Group, as `sonos-fake` records them.
    const CONTROLS: [&str; 5] = [
        "AVTransport#Pause",
        "AVTransport#Play",
        "AVTransport#Seek",
        "GroupRenderingControl#SnapshotGroupVolume",
        "GroupRenderingControl#SetGroupVolume",
    ];

    fn controls(household: &Household, player: &PlayerId) -> Vec<String> {
        let actions = household.actions_of(player);
        let controls = actions
            .into_iter()
            .filter(|a| CONTROLS.contains(&a.as_str()));
        controls.collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_group_is_paused_resumed_sought_and_its_volume_set_on_its_coordinator() {
        let (household, clock) = Household::with_manual_clock();
        let kitchen = household.add_player("Kitchen");
        let bath = household.add_player("Bath");
        household.join(&bath, &kitchen);
        household.play(
            &kitchen,
            FakeTrack::new("Moby Dick", Duration::from_secs(600)),
        );
        clock.advance(Duration::from_secs(90));
        let shared = knowing(&household, &clock).await;
        let control = async |control| {
            shared
                .control(kitchen.uuid(), control, soon(CONTROL_TIMEOUT))
                .await
        };

        let playback = shared
            .playback(kitchen.uuid(), soon(CONTROL_TIMEOUT))
            .await
            .unwrap();
        assert_eq!(
            playback,
            Playback {
                state: TransportState::Playing,
                uri: Some("http://127.0.0.1:1/track.mp3".into()),
                position: Some(Duration::from_secs(90)),
                duration: Some(Duration::from_secs(600)),
            }
        );

        control(Control::Pause).await.unwrap();
        assert_eq!(household.transport_state(&kitchen), "PAUSED_PLAYBACK");
        clock.advance(Duration::from_secs(60));
        assert_eq!(household.position(&kitchen), Duration::from_secs(90));

        // A fraction of a second is dropped.
        control(Control::Seek(Duration::from_millis(125_900)))
            .await
            .unwrap();
        assert_eq!(household.position(&kitchen), Duration::from_secs(125));
        assert_eq!(household.transport_state(&kitchen), "PAUSED_PLAYBACK");

        control(Control::Resume).await.unwrap();
        assert_eq!(household.transport_state(&kitchen), "PLAYING");
        clock.advance(Duration::from_secs(5));
        assert_eq!(household.position(&kitchen), Duration::from_secs(130));

        control(Control::SetVolume(42)).await.unwrap();
        assert_eq!(household.volume(&kitchen), 42);

        assert_eq!(
            controls(&household, &kitchen),
            [
                "AVTransport#Pause",
                "AVTransport#Seek",
                "AVTransport#Play",
                "GroupRenderingControl#SnapshotGroupVolume",
                "GroupRenderingControl#SetGroupVolume",
            ]
        );
        // Nothing went to the member.
        assert_eq!(controls(&household, &bath), Vec::<String>::new());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_discovery_lapse_is_unknown_topology_even_after_an_event_wake() {
        let (household, clock) = Household::with_manual_clock();
        household.add_player("Kitchen");
        let upnp = Upnp {
            shared: Arc::new(knowing(&household, &clock).await),
            discovery: tokio::spawn(async {}).abort_handle(),
            event_task: Mutex::new(None),
        };
        assert_eq!(Sonos::topology(&upnp).await.unwrap().len(), 1);
        let mut hints = upnp.shared.events.changed.subscribe();
        clock.advance(EXPIRY + Duration::from_secs(1));
        upnp.shared
            .events
            .changed
            .send_modify(|version| *version += 1);
        hints.changed().await.unwrap();
        assert_eq!(Sonos::topology(&upnp).await, Err(Error::SearchFailed));
        // Discovery recovers: current topology is available again.
        upnp.shared
            .saw(Ok(ssdp::search(household.ssdp()).await.unwrap()));
        assert_eq!(Sonos::topology(&upnp).await.unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_highest_volume_is_sent_for_anything_louder() {
        let (household, clock) = Household::with_manual_clock();
        let kitchen = household.add_player("Kitchen");
        let upnp = Upnp {
            shared: Arc::new(knowing(&household, &clock).await),
            discovery: tokio::spawn(async {}).abort_handle(),
            event_task: Mutex::new(None),
        };

        Sonos::set_volume(&upnp, kitchen.uuid(), 255).await.unwrap();
        assert_eq!(household.volume(&kitchen), MAX_VOLUME);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_a_group_in_the_topology_is_controlled() {
        let (household, clock) = Household::with_manual_clock();
        let kitchen = household.add_player("Kitchen");
        let bath = household.add_player("Bath");
        household.join(&bath, &kitchen);
        let shared = knowing(&household, &clock).await;

        for group in [bath.uuid(), "RINCON_NOSUCH01400"] {
            assert_eq!(
                shared.playback(group, soon(CONTROL_TIMEOUT)).await,
                Err(Error::UnknownGroup),
                "{group}"
            );
            for control in [
                Control::Pause,
                Control::Resume,
                Control::Seek(Duration::ZERO),
                Control::SetVolume(10),
            ] {
                assert_eq!(
                    shared.control(group, control, soon(CONTROL_TIMEOUT)).await,
                    Err(Error::UnknownGroup),
                    "{group} {control:?}"
                );
            }
        }
        for player in [&kitchen, &bath] {
            assert_eq!(controls(&household, player), Vec::<String>::new());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_coordinator_that_refuses_or_hangs_a_control_fails_in_time() {
        let (household, clock) = Household::with_manual_clock();
        let kitchen = household.add_player("Kitchen");
        household.play(
            &kitchen,
            FakeTrack::new("Moby Dick", Duration::from_secs(600)),
        );
        household.set_fault(&kitchen, Fault::RefuseControl);
        let office = household.add_player("Office");
        household.set_fault(&office, Fault::Hang);
        let shared = knowing(&household, &clock).await;

        for control in [
            Control::Pause,
            Control::Resume,
            Control::Seek(Duration::from_secs(5)),
            Control::SetVolume(10),
        ] {
            assert_eq!(
                shared
                    .control(kitchen.uuid(), control, soon(CONTROL_TIMEOUT))
                    .await,
                Err(Error::GroupFailed),
                "{control:?}"
            );
        }
        assert_eq!(household.transport_state(&kitchen), "PLAYING");
        assert_eq!(household.volume(&kitchen), sonos_fake::DEFAULT_VOLUME);

        // The topology comes from Kitchen; Office never answers its own calls.
        let start = tokio::time::Instant::now();
        assert_eq!(
            shared
                .control(office.uuid(), Control::Pause, soon(Duration::from_secs(1)))
                .await,
            Err(Error::GroupFailed)
        );
        assert_eq!(
            shared
                .playback(office.uuid(), soon(Duration::from_secs(1)))
                .await,
            Err(Error::GroupFailed)
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1800) && elapsed < Duration::from_secs(3),
            "{elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_group_with_nothing_loaded_tells_no_track() {
        let (household, clock) = Household::with_manual_clock();
        let kitchen = household.add_player("Kitchen");
        let shared = knowing(&household, &clock).await;

        assert_eq!(
            shared
                .playback(kitchen.uuid(), soon(CONTROL_TIMEOUT))
                .await
                .unwrap(),
            Playback {
                state: TransportState::Stopped,
                uri: None,
                position: None,
                duration: None,
            }
        );
    }
}
