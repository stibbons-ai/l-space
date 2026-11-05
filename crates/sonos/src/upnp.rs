//! [`Sonos`] over local UPnP: players found with SSDP, the Group topology read from any of
//! them (ZoneGroupTopology `GetZoneGroupState`), and each Group's playback from its coordinator
//! (AVTransport `GetTransportInfo` and `GetPositionInfo`).

use std::collections::BTreeMap;
use std::net::SocketAddr;
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
use crate::{Error, Group, Member, Sonos, Track, TransportState, ssdp};

/// How long after one search the next one starts, on the [`Clock`].
pub const REFRESH: Duration = Duration::from_secs(60);

/// How long a player stays known after it last answered a search, on the [`Clock`]: three
/// searches, so one lost datagram doesn't drop it.
pub const EXPIRY: Duration = Duration::from_secs(3 * 60);

/// How long [`Upnp::groups`] may take in all, waiting for the first search included; shorter
/// than the App's limit for a call to the Bridge.
pub const LISTING_TIMEOUT: Duration = Duration::from_secs(8);

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
}

#[derive(Debug)]
struct Shared {
    http: Client,
    clock: Arc<dyn Clock>,
    known: Mutex<Known>,
    /// Whether the first search is over (or searching stopped for good).
    searched: watch::Sender<bool>,
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
        }
    }

    async fn groups(&self) -> Result<Vec<Group>, Error> {
        let deadline = tokio::time::Instant::now() + LISTING_TIMEOUT;
        self.shared.groups(deadline).await
    }
}

impl Drop for Upnp {
    fn drop(&mut self) {
        self.discovery.abort();
    }
}

impl Sonos for Upnp {
    fn groups(&self) -> BoxFuture<'_, Result<Vec<Group>, Error>> {
        Box::pin(self.groups())
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
            http,
            clock,
            known: Mutex::default(),
            searched: watch::Sender::new(false),
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
                    Ok(Vec::new())
                };
            }
            let candidates = candidates(&players, &known.ranks);
            (players, candidates)
        };
        let Ok(topology) = tokio::time::timeout_at(deadline, self.topology(&candidates)).await
        else {
            tracing::warn!(
                timeout = ?LISTING_TIMEOUT,
                "no Sonos player told the Group topology within the listing's time"
            );
            return Err(Error::Unreachable);
        };
        let (from, topology) = topology?;
        self.known().ranks = ranks(&from, &topology);
        let mut groups = join_all(topology.iter().filter_map(visible).map(|group| {
            // Only an address a player announced for itself: the topology's could point anywhere.
            let coordinator = players.get(&group.id).copied();
            self.with_playback(group, coordinator, deadline)
        }))
        .await;
        groups.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(groups)
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

    /// `group` with its transport state and track, as its coordinator (at `coordinator`, if
    /// found by a search) tells them by `deadline`. Each is kept if it came in time; a track
    /// without a state is not shown.
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
        let call = |action| async move {
            tokio::time::timeout_at(
                deadline,
                soap::call(&self.http, coordinator, Service::AvTransport, action),
            )
            .await
        };
        let (transport, position) = tokio::join!(call("GetTransportInfo"), call("GetPositionInfo"));
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
        group
    }
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
        duration: argument(answer, "TrackDuration")
            .and_then(xml::duration)
            .filter(|duration| !duration.is_zero()),
    })
}

#[cfg(test)]
mod tests {
    use clock::ManualClock;
    use sonos_fake::{FakeTrack, Fault, Household};

    use super::*;
    use crate::xml::ZoneMember;

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

        // Asking each in turn would take 2.5 s of stagger and 2 s for the last to time out.
        let start = tokio::time::Instant::now();
        assert_eq!(
            shared.groups(soon(Duration::from_secs(1))).await,
            Err(Error::Unreachable)
        );
        assert!(
            start.elapsed() < Duration::from_millis(1300),
            "{:?}",
            start.elapsed()
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
}
