//! Sonos control behind one trait, [`Sonos`] (ADR 0002, ADR 0005), so the official cloud
//! Control API can replace the local UPnP implementation, [`Upnp`], later.
//!
//! So far it only reads: every Group as set up in the Sonos app, with its members and what its
//! coordinator plays. It never changes playback, volume or grouping.
//!
//! Every SSDP datagram and SOAP body sent or received is logged at DEBUG under the target
//! [`RAW`], and only there; INFO and above summarise without raw bodies.

mod soap;
mod ssdp;
mod upnp;
mod xml;

use std::time::Duration;

use futures::future::BoxFuture;

pub use soap::MAX_BODY;
pub use ssdp::MULTICAST;
pub use upnp::{EXPIRY, LISTING_TIMEOUT, REFRESH, Upnp};

/// The `tracing` target of the raw UPnP traffic: each SSDP datagram and SOAP request and
/// response body, at DEBUG. Enable it with `RUST_LOG=sonos::raw=debug`.
pub const RAW: &str = "sonos::raw";

/// Sonos, as the Bridge uses it.
pub trait Sonos: std::fmt::Debug + Send + Sync + 'static {
    /// Every Group as the Sonos app shows it, by name. A household where a working search found
    /// no player has none.
    fn groups(&self) -> BoxFuture<'_, Result<Vec<Group>, Error>>;
}

/// Why Sonos could not answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Players are known, but none of them told the Group topology within the listing's time.
    #[error("no Sonos player told the Group topology")]
    Unreachable,
    /// No player is known, and whether there are any is too: the last search could not be
    /// sent (its socket not bound, or its datagram not sent), searching stopped, or the first
    /// search didn't end within the listing's time.
    #[error("cannot search for Sonos players")]
    SearchFailed,
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
    /// The current track, if one is loaded and the coordinator said so.
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
    /// The coordinator's answer had no state, or one UPnP doesn't define.
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
