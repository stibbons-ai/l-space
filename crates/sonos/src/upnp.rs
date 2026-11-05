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
use tokio::task::JoinHandle;

use crate::soap::{self, CallError, Service, argument};
use crate::xml::{self, ZoneGroup};
use crate::{Error, Group, Member, Sonos, Track, ssdp};

/// How long after one search the next one starts, on the [`Clock`].
pub const REFRESH: Duration = Duration::from_secs(60);

/// How long a player stays known after it last answered a search, on the [`Clock`]: three
/// searches, so one lost datagram doesn't drop it.
pub const EXPIRY: Duration = Duration::from_secs(3 * 60);

/// How long one SOAP call may take, connecting included.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// How long after asking one player for the topology the next is asked too, while the first
/// hasn't answered: a player that stopped answering only delays the listing by this much.
const STAGGER: Duration = Duration::from_millis(500);

/// How long [`Upnp::groups`] may take in all, waiting for the first search included; shorter
/// than the App's limit for a call to the Bridge.
const LISTING_TIMEOUT: Duration = Duration::from_secs(8);

/// Sonos over local UPnP. Searching for players starts with [`Upnp::start`] and repeats every
/// [`REFRESH`] until this is dropped.
#[derive(Debug)]
pub struct Upnp {
    shared: Arc<Shared>,
    discovery: JoinHandle<()>,
}

#[derive(Debug)]
struct Shared {
    http: Client,
    clock: Arc<dyn Clock>,
    known: Mutex<Known>,
    /// Whether the first search is over.
    searched: watch::Sender<bool>,
}

/// What the searches and listings found out.
#[derive(Debug, Default)]
struct Known {
    /// The players found, by UUID; at most [`ssdp::MAX_PLAYERS`].
    players: BTreeMap<String, Player>,
    /// Whether the last search could not be sent.
    search_failed: bool,
    /// How promising each player is to ask for the topology, from the last one read (lower is
    /// better; see [`rank`]).
    ranks: BTreeMap<String, u8>,
}

#[derive(Debug)]
struct Player {
    /// Where its UPnP services answer, as it announced.
    addr: SocketAddr,
    /// When it last answered a search.
    seen: Instant,
}

impl Upnp {
    /// Starts searching for players by sending M-SEARCH to `ssdp` (normally [`crate::MULTICAST`]),
    /// timed by `clock`. Must be called on a Tokio runtime.
    pub fn start(ssdp: SocketAddr, clock: Arc<dyn Clock>) -> Self {
        let http = Client::builder()
            .timeout(CALL_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .expect("an HTTP client without TLS builds");
        let shared = Arc::new(Shared {
            http,
            clock,
            known: Mutex::default(),
            searched: watch::Sender::new(false),
        });
        let discovery = tokio::spawn(discover(Arc::clone(&shared), ssdp));
        Self { shared, discovery }
    }

    async fn groups(&self) -> Result<Vec<Group>, Error> {
        if self.discovery.is_finished() {
            // It loops forever, so it panicked: the players known stay as they were.
            tracing::error!("the search for Sonos players stopped");
        }
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
        shared.searched.send_replace(true);
        shared.clock.sleep(REFRESH).await;
    }
}

impl Shared {
    fn known(&self) -> MutexGuard<'_, Known> {
        self.known.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the players a search found (or that it failed), and forgets those unseen for
    /// [`EXPIRY`].
    fn saw(&self, found: Result<Vec<ssdp::Found>, ()>) {
        let now = self.clock.now();
        let mut known = self.known();
        known.search_failed = found.is_err();
        let players = &mut known.players;
        let before = players.len();
        let mut added = 0;
        for player in found.unwrap_or_default() {
            if let Some(known) = players.get_mut(&player.uuid) {
                *known = Player {
                    addr: player.addr,
                    seen: now,
                };
            } else if players.len() < ssdp::MAX_PLAYERS {
                let addr = player.addr;
                players.insert(player.uuid, Player { addr, seen: now });
                added += 1;
            }
        }
        players.retain(|_, player| now.duration_since(player.seen) < EXPIRY);
        // Found players were just seen, so only those not found can have been forgotten.
        if added > 0 || players.len() < before + added {
            let uuids: Vec<&String> = players.keys().collect();
            tracing::info!(players = ?uuids, "Sonos players changed");
        }
    }

    /// Every Group, if the topology could be read by `deadline`. A Group whose coordinator
    /// didn't tell its playback by then is listed without it.
    async fn groups(&self, deadline: tokio::time::Instant) -> Result<Vec<Group>, Error> {
        // Without a first search, an empty list would only mean "not searched yet".
        let mut searched = self.searched.subscribe();
        if tokio::time::timeout_at(deadline, searched.wait_for(|searched| *searched))
            .await
            .is_err()
        {
            tracing::warn!("the first search for Sonos players did not end in time");
            return Err(Error::Unreachable);
        }
        let (players, candidates) = {
            let known = self.known();
            if known.players.is_empty() {
                return if known.search_failed {
                    Err(Error::SearchFailed)
                } else {
                    Ok(Vec::new())
                };
            }
            let players: BTreeMap<String, SocketAddr> = known
                .players
                .iter()
                .map(|(uuid, player)| (uuid.clone(), player.addr))
                .collect();
            let mut candidates: Vec<(&String, &SocketAddr)> = players.iter().collect();
            candidates.sort_by_key(|(uuid, _)| known.ranks.get(*uuid).copied().unwrap_or(RANK_NEW));
            let candidates: Vec<(String, SocketAddr)> = candidates
                .into_iter()
                .map(|(uuid, addr)| (uuid.clone(), *addr))
                .collect();
            (players, candidates)
        };
        let Ok(topology) = tokio::time::timeout_at(deadline, self.topology(&candidates)).await
        else {
            tracing::warn!(timeout = ?LISTING_TIMEOUT, "no Sonos player told the Group topology in time");
            return Err(Error::Unreachable);
        };
        let (from, topology) = topology?;
        self.known().ranks = ranks(&from, &topology);
        let mut groups = join_all(topology.iter().filter_map(visible).map(|group| {
            // Only an address a player announced for itself: the topology's could point anywhere.
            let coordinator = players.get(&group.id).copied();
            async move {
                let id = group.id.clone();
                let without_playback = group.clone();
                tokio::time::timeout_at(deadline, self.with_playback(group, coordinator))
                    .await
                    .unwrap_or_else(|_| {
                        tracing::warn!(
                            group = id,
                            "a Group's coordinator did not tell its playback in time"
                        );
                        without_playback
                    })
            }
        }))
        .await;
        groups.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(groups)
    }

    /// The Group topology and the UUID of the player that told it, asking `candidates` (UUID,
    /// address) in order. Every player knows the whole household's. A player that hasn't
    /// answered after [`STAGGER`] doesn't hold up asking the next.
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
    /// found by a search) tells them.
    async fn with_playback(&self, mut group: Group, coordinator: Option<SocketAddr>) -> Group {
        let Some(coordinator) = coordinator else {
            tracing::warn!(
                group = group.id,
                "a Group's coordinator did not answer the last searches"
            );
            return group;
        };
        let (transport, position) = tokio::join!(
            soap::call(
                &self.http,
                coordinator,
                Service::AvTransport,
                "GetTransportInfo"
            ),
            soap::call(
                &self.http,
                coordinator,
                Service::AvTransport,
                "GetPositionInfo"
            ),
        );
        match transport {
            Ok(answer) => {
                group.state = argument(&answer, "CurrentTransportState").map(xml::transport_state);
            }
            Err(error) => {
                tracing::warn!(group = group.id, %error, "cannot read a Group's transport state")
            }
        }
        match position {
            Ok(answer) => group.track = track(&answer),
            Err(error) => tracing::warn!(group = group.id, %error, "cannot read a Group's track"),
        }
        group
    }
}

/// The rank of a player not in the last topology read.
const RANK_NEW: u8 = 3;

/// How promising each player in `topology` is to ask for it next time (lower is better): the
/// one that told it (`from`) first, then the visible coordinators, the other visible members,
/// and last the invisible ones, since bonded satellites can tell an outdated topology (SoCo,
/// `zonegroupstate.py`).
fn ranks(from: &str, topology: &[ZoneGroup]) -> BTreeMap<String, u8> {
    let mut ranks = BTreeMap::new();
    for zone in topology {
        for member in &zone.members {
            let rank = match (member.visible, member.uuid == zone.coordinator) {
                (true, true) => 1,
                (true, false) => 2,
                (false, _) => 4,
            };
            ranks.insert(member.uuid.clone(), rank);
        }
    }
    ranks.insert(from.to_owned(), 0);
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
                member("RINCON_L", "Living Room", true),
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

    #[test]
    fn the_player_that_told_the_topology_is_asked_first_and_satellites_last() {
        let topology = [
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
        ];
        let ranks = ranks("RINCON_B", &topology);
        let mut order: Vec<&String> = ranks.keys().collect();
        order.sort_by_key(|uuid| ranks[*uuid]);
        assert_eq!(order, ["RINCON_B", "RINCON_K", "RINCON_T", "RINCON_S"]);
        assert!(ranks["RINCON_S"] > RANK_NEW);
    }
}
