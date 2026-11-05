//! Sonos control behind one trait, [`Sonos`] (ADR 0002, ADR 0005), so the official cloud
//! Control API can replace the local UPnP implementation, [`Upnp`], later.
//!
//! It reads every Group as set up in the Sonos app, with its members and what its coordinator
//! plays, and starts a stream the Bridge serves on a Group ([`Sonos::play`]). It never changes
//! volume or grouping.
//!
//! Every SSDP datagram and SOAP body sent or received is logged at DEBUG under the target
//! [`RAW`], and only there; INFO and above summarise without raw bodies.

mod soap;
mod ssdp;
mod upnp;
mod xml;

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use futures::future::BoxFuture;

pub use soap::MAX_BODY;
pub use ssdp::MULTICAST;
pub use upnp::{EXPIRY, LISTING_TIMEOUT, PLAY_TIMEOUT, REFRESH, Upnp};

/// The `tracing` target of the raw UPnP traffic: each SSDP datagram and SOAP request and
/// response body, at DEBUG. Enable it with `RUST_LOG=sonos::raw=debug`.
pub const RAW: &str = "sonos::raw";

/// Sonos, as the Bridge uses it.
pub trait Sonos: std::fmt::Debug + Send + Sync + 'static {
    /// Every Group as the Sonos app shows it, by name. A household where a working search found
    /// no player has none.
    fn groups(&self) -> BoxFuture<'_, Result<Vec<Group>, Error>>;

    /// Group `group` (its id) plays `stream` from its start, replacing what it played. Returns
    /// the URL the Group was given.
    fn play<'a>(
        &'a self,
        group: &'a str,
        stream: &'a Stream,
    ) -> BoxFuture<'a, Result<String, Error>>;
}

/// A stream the Bridge serves over plain HTTP, for a Group to play.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    /// Where the Bridge listens. An unspecified address (`0.0.0.0`, `::`) listens on all of
    /// the host's; the Group is then given the one on the route to its coordinator.
    pub listen: SocketAddr,
    /// The stream's path, starting with `/`, already percent-encoded as needed.
    pub path: String,
}

impl Stream {
    /// The `http` URL of the stream on `host`, the address the Group reaches the Bridge on
    /// when the Bridge listens on all of its addresses.
    pub fn url(&self, host: IpAddr) -> String {
        let ip = if self.listen.ip().is_unspecified() {
            host
        } else {
            self.listen.ip()
        };
        format!(
            "http://{}{}",
            SocketAddr::new(ip, self.listen.port()),
            self.path
        )
    }
}

/// Why Sonos could not answer, or not do what was asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Players are known, but none of them told the Group topology within the time allowed.
    #[error("no Sonos player told the Group topology")]
    Unreachable,
    /// No player is known, and the Bridge can't tell whether there are any: the last search
    /// could not be sent (its socket not bound, or its datagram not sent), searching stopped,
    /// or the first search didn't end within the time allowed.
    #[error("cannot search for Sonos players")]
    SearchFailed,
    /// The household has no Group with this id: none in the topology just read is coordinated
    /// by it and has a member the Sonos app shows.
    #[error("no such Sonos Group")]
    UnknownGroup,
    /// The Group's coordinator was not found by the last searches, didn't answer in time, or
    /// refused (a SOAP fault).
    #[error("the Group's coordinator did not start playing")]
    GroupFailed,
}

/// One or more players playing in sync, as grouped in the Sonos app.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// The coordinator's UUID, such as `RINCON_48A6B8000001400`, always, also when the
    /// coordinator is not a member (it is invisible in the Sonos app).
    pub id: String,
    /// The name set in the Sonos app for the coordinator (bonded speakers share it), or for the
    /// first member if the coordinator has none.
    pub name: String,
    /// The players the Sonos app shows in the Group, the coordinator first when it is one of
    /// them; never empty. A bonded surround or sub, the second speaker of a stereo pair and a
    /// Boost are not members.
    pub members: Vec<Member>,
    /// What the Group's transport is doing; none if its coordinator was not found by the
    /// searches, didn't answer, or answered too late.
    pub state: Option<TransportState>,
    /// The current track, if one is loaded and the coordinator said so; none whenever `state`
    /// is none.
    pub track: Option<Track>,
}

/// A player in a Group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Its UUID.
    pub id: String,
    /// The name set in the Sonos app for it.
    pub name: String,
}

/// A Group's transport state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportState {
    Playing,
    Paused,
    Stopped,
    Transitioning,
    /// The coordinator's answer had no state, or one the Bridge doesn't know.
    Unknown,
}

/// The track a Group has loaded.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    /// From the track's metadata, if it has one.
    pub title: Option<String>,
    pub creator: Option<String>,
    /// How far into the track playback is.
    pub position: Option<Duration>,
    /// The track's length; none when unknown, as for a stream.
    pub duration: Option<Duration>,
}
