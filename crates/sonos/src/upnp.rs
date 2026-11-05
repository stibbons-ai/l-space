//! [`Sonos`] over local UPnP: players found with SSDP, the Group topology read from any of
//! them (ZoneGroupTopology `GetZoneGroupState`), and each Group's playback from its coordinator
//! (AVTransport `GetTransportInfo` and `GetPositionInfo`).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use clock::Clock;
use futures::future::{BoxFuture, join_all};
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

/// How long a search waits for answers; players answer within a second (`MX: 1`).
const SEARCH_WINDOW: Duration = Duration::from_secs(1);

/// How long one SOAP call may take, connecting included.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

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
    /// The players found, by UUID.
    players: Mutex<BTreeMap<String, Player>>,
    /// Whether the first search is over.
    searched: watch::Sender<bool>,
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
            players: Mutex::default(),
            searched: watch::Sender::new(false),
        });
        let discovery = tokio::spawn(discover(Arc::clone(&shared), ssdp));
        Self { shared, discovery }
    }

    async fn groups(&self) -> Result<Vec<Group>, Error> {
        tokio::time::timeout(LISTING_TIMEOUT, self.shared.groups())
            .await
            .unwrap_or_else(|_| {
                tracing::warn!(timeout = ?LISTING_TIMEOUT, "Sonos players did not answer in time");
                Err(Error::Unreachable)
            })
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
        let found = ssdp::search(target, SEARCH_WINDOW)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%target, %error, "SSDP search failed");
                Vec::new()
            });
        shared.saw(found);
        shared.searched.send_replace(true);
        shared.clock.sleep(REFRESH).await;
    }
}

impl Shared {
    /// Records the players a search found, and forgets those unseen for [`EXPIRY`].
    fn saw(&self, found: Vec<ssdp::Found>) {
        let now = self.clock.now();
        let mut players = self.players.lock().unwrap_or_else(PoisonError::into_inner);
        let known = players.len();
        let mut added = 0;
        for player in found {
            let addr = player.addr;
            if players
                .insert(player.uuid, Player { addr, seen: now })
                .is_none()
            {
                added += 1;
            }
        }
        players.retain(|_, player| now.duration_since(player.seen) < EXPIRY);
        // Found players were just seen, so only those not found can have been forgotten.
        if added > 0 || players.len() < known + added {
            let uuids: Vec<&String> = players.keys().collect();
            tracing::info!(players = ?uuids, "Sonos players changed");
        }
    }

    async fn groups(&self) -> Result<Vec<Group>, Error> {
        // Without a first search, an empty list would only mean "not searched yet".
        let _ = self
            .searched
            .subscribe()
            .wait_for(|searched| *searched)
            .await;
        let players: BTreeMap<String, SocketAddr> = self
            .players
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(uuid, player)| (uuid.clone(), player.addr))
            .collect();
        if players.is_empty() {
            return Ok(Vec::new());
        }
        let topology = self.topology(players.values()).await?;
        let mut groups = join_all(topology.iter().filter_map(visible).map(|group| {
            // Only an address a player announced for itself: the topology's could point anywhere.
            let coordinator = players.get(&group.id).copied();
            self.with_playback(group, coordinator)
        }))
        .await;
        groups.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(groups)
    }

    /// The Group topology, from the first of `players` that tells it. Every player knows the
    /// whole household's.
    async fn topology(
        &self,
        players: impl Iterator<Item = &SocketAddr>,
    ) -> Result<Vec<ZoneGroup>, Error> {
        for &player in players {
            match self.zone_groups(player).await {
                Ok(topology) => return Ok(topology),
                Err(error) => {
                    tracing::warn!(%player, %error, "cannot read the Group topology");
                }
            }
        }
        Err(Error::Unreachable)
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
                group.state =
                    argument(&answer, "CurrentTransportState").and_then(xml::transport_state);
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
    // Bonded speakers share their room's name, so an invisible coordinator still names it.
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
