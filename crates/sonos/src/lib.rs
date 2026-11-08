//! Sonos control behind one trait, [`Sonos`] (ADR 0002, ADR 0005), so the official cloud
//! Control API can replace the local UPnP implementation, [`Upnp`], later.
//!
//! It reads every Group as set up in the Sonos app, with its members, what its coordinator
//! plays and the Group's volume; starts a stream the Bridge serves on a Group
//! ([`Sonos::play`]); and controls what a Group plays: pause, resume, seek, and the Group's
//! volume. [`media_type`] tells which audio formats a Group plays; `play` doesn't check, the
//! caller does. It never changes grouping.
//!
//! Every SSDP datagram and SOAP body sent or received is logged at DEBUG under the target
//! [`RAW`], and only there; INFO and above summarise without raw bodies.

mod media;
mod soap;
mod ssdp;
mod upnp;
mod xml;

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use futures::future::BoxFuture;

pub use media::media_type;
pub use soap::MAX_BODY;
pub use ssdp::MULTICAST;
pub use upnp::{CONTROL_TIMEOUT, EXPIRY, LISTING_TIMEOUT, PLAY_TIMEOUT, REFRESH, Upnp};

/// The `tracing` target of the raw UPnP traffic: each SSDP datagram and SOAP request and
/// response body, at DEBUG. Enable it with `RUST_LOG=sonos::raw=debug`.
pub const RAW: &str = "sonos::raw";

/// The highest volume a Group takes; the lowest is 0.
pub const MAX_VOLUME: u8 = 100;

/// Sonos, as the Bridge uses it.
///
/// Every call on one Group acts on its coordinator as the topology read for that call names
/// it: a Group the topology doesn't have is [`Error::UnknownGroup`]; a coordinator the
/// searches didn't find, or that doesn't answer in time or refuses, is [`Error::GroupFailed`].
pub trait Sonos: std::fmt::Debug + Send + Sync + 'static {
    /// Every Group as the Sonos app shows it, by name. A household where a working search found
    /// no player has none.
    fn groups(&self) -> BoxFuture<'_, Result<Vec<Group>, Error>>;

    /// Group `group` (its id) plays `stream` from its start, replacing what it played.
    fn play<'a>(&'a self, group: &'a str, stream: &'a Stream) -> BoxFuture<'a, Result<(), Error>>;

    /// What Group `group` plays now, as its coordinator tells it.
    fn playback<'a>(&'a self, group: &'a str) -> BoxFuture<'a, Result<Playback, Error>>;

    /// Group `group` pauses where it is.
    fn pause<'a>(&'a self, group: &'a str) -> BoxFuture<'a, Result<(), Error>>;

    /// Group `group` plays on from where it is, at normal speed.
    fn resume<'a>(&'a self, group: &'a str) -> BoxFuture<'a, Result<(), Error>>;

    /// Group `group` goes to `position` into its current track, in whole seconds (any
    /// fraction is dropped), playing or paused as it was.
    fn seek<'a>(&'a self, group: &'a str, position: Duration) -> BoxFuture<'a, Result<(), Error>>;

    /// Group `group`'s volume becomes `volume`, at most [`MAX_VOLUME`]: every member's
    /// changes as the Sonos app's Group volume changes them.
    fn set_volume<'a>(&'a self, group: &'a str, volume: u8) -> BoxFuture<'a, Result<(), Error>>;
}

/// A stream the Bridge serves over plain HTTP, for a Group to play. Its path is a capability,
/// so `Debug` leaves it out.
#[derive(Clone, PartialEq, Eq)]
pub struct Stream {
    /// Where the Bridge listens. An unspecified address (`0.0.0.0`, `::`) listens on all of
    /// the host's; the Group is then given the one on the route to its coordinator.
    pub listen: SocketAddr,
    /// The stream's path, starting with `/`, already percent-encoded as needed.
    pub path: String,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream")
            .field("listen", &self.listen)
            .field("path", &"REDACTED")
            .finish()
    }
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
    /// refused (a SOAP fault), or answered without what was asked for.
    #[error("the Group's coordinator did not do what was asked")]
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
    /// The Group's volume, 0 to [`MAX_VOLUME`]; none if its coordinator didn't tell it in
    /// time, refused to, or told something that isn't such a number.
    pub volume: Option<u8>,
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

/// What a Group plays, as [`Sonos::playback`] reads it from its coordinator.
#[derive(Clone, PartialEq)]
pub struct Playback {
    pub state: TransportState,
    /// The URL of the track it has loaded; none if nothing is. For a stream the Bridge serves,
    /// it carries the stream's capability, so `Debug` leaves it out.
    pub uri: Option<String>,
    /// How far into the track playback is; none if nothing is loaded or the coordinator didn't
    /// say.
    pub position: Option<Duration>,
    /// The track's length; none when unknown, as for a stream.
    pub duration: Option<Duration>,
}

impl std::fmt::Debug for Playback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Playback")
            .field("state", &self.state)
            .field("uri", &self.uri.as_ref().map(|_| "REDACTED"))
            .field("position", &self.position)
            .field("duration", &self.duration)
            .finish()
    }
}
