//! GENA subscriptions. Bodies only wake readers: delayed notifications cannot apply old
//! transport or topology to a newer Handoff. SIDs and URLs are never logged.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::{EXPIRY, Shared, route_to};
use crate::Notification;

const SERVICES: [&str; 2] = [
    "/MediaRenderer/AVTransport/Event",
    "/ZoneGroupTopology/Event",
];
const RETRY: Duration = Duration::from_secs(5);

struct Subscription {
    sid: String,
    sequence: Option<u32>,
    expires: Option<Instant>,
    value: Option<String>,
    renew: Option<Instant>,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("sequence", &self.sequence)
            .field("expires", &self.expires)
            .field("renew", &self.renew)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
pub(super) struct Events {
    subscriptions: Mutex<BTreeMap<(SocketAddr, &'static str), Subscription>>,
    pub(super) changed: watch::Sender<u64>,
}

impl Events {
    fn wake(&self) {
        self.changed
            .send_modify(|value| *value = value.wrapping_add(1));
    }

    pub(super) fn notify(&self, sid: &str, seq: u32, body: &str, now: Instant) -> Notification {
        if body.len() > crate::MAX_BODY {
            tracing::debug!("oversized Sonos notification");
            return Notification::Malformed;
        }
        let Ok(document) = crate::xml::parse(body) else {
            tracing::debug!("malformed Sonos notification XML");
            return Notification::Malformed;
        };
        if !document
            .root_element()
            .has_tag_name(("urn:schemas-upnp-org:event-1-0", "propertyset"))
        {
            return Notification::Malformed;
        }
        let mut subscriptions = self
            .subscriptions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(subscription) = subscriptions
            .values_mut()
            .find(|entry| entry.sid == sid && entry.expires.is_none_or(|expires| expires > now))
        else {
            tracing::debug!("Sonos notification for unknown or expired subscription");
            return Notification::UnknownSubscription;
        };
        if subscription.sequence.is_some_and(|previous| {
            let distance = seq.wrapping_sub(previous);
            distance == 0 || distance >= (1 << 31)
        }) {
            tracing::debug!("duplicate or backward Sonos notification acknowledged");
            return Notification::Ignored;
        }
        subscription.sequence = Some(seq);
        let value = event_value(&document);
        let changed = value
            .as_ref()
            .is_some_and(|value| subscription.value.as_ref() != Some(value));
        if value.is_some() {
            subscription.value = value;
        }
        drop(subscriptions);
        if changed {
            self.wake();
        }
        Notification::Accepted
    }
}

/// Ignore volume, position and transport-only notifications, not a time window. A source
/// change concurrent with a Bridge control still changes its URI and wakes reconciliation.
fn event_value(document: &roxmltree::Document<'_>) -> Option<String> {
    if let Some(topology) = document
        .descendants()
        .find(|node| node.has_tag_name("ZoneGroupState"))
    {
        return topology.text().map(str::to_owned);
    }
    let change = document
        .descendants()
        .find(|node| node.has_tag_name("LastChange"))?
        .text()?;
    let change = crate::xml::parse(change).ok()?;
    let instance = change
        .descendants()
        .find(|node| node.has_tag_name("InstanceID") && node.attribute("val") == Some("0"))?;
    instance
        .children()
        .find(|node| node.has_tag_name("AVTransportURI"))
        .or_else(|| {
            instance
                .children()
                .find(|node| node.has_tag_name("CurrentTrackURI"))
        })?
        .attribute("val")
        .map(str::to_owned)
}

/// An infinite grant has neither an expiration nor a renewal deadline.
fn grant(headers: &reqwest::header::HeaderMap) -> Result<(String, Option<Duration>), &'static str> {
    let sid = headers
        .get("SID")
        .ok_or("missing SID")?
        .to_str()
        .map_err(|_| "invalid SID")?;
    if sid.is_empty() || sid.len() > 256 {
        return Err("invalid SID");
    }
    let timeout = headers
        .get("TIMEOUT")
        .ok_or("missing TIMEOUT")?
        .to_str()
        .map_err(|_| "invalid TIMEOUT")?;
    let lifetime = if timeout.eq_ignore_ascii_case("Second-infinite") {
        None
    } else {
        let seconds = timeout
            .strip_prefix("Second-")
            .ok_or("invalid TIMEOUT")?
            .parse::<u32>()
            .map_err(|_| "invalid TIMEOUT")?;
        if seconds == 0 {
            return Err("zero TIMEOUT");
        }
        Some(Duration::from_secs(u64::from(seconds)))
    };
    Ok((sid.to_owned(), lifetime))
}

pub(super) async fn run(shared: Arc<Shared>, listen: SocketAddr, clock: Arc<dyn clock::Clock>) {
    let mut searched = shared.searched.subscribe();
    loop {
        let now = clock.now();
        let players: Vec<_> = shared
            .known()
            .players
            .values()
            .filter(|player| shared.clock.now().saturating_duration_since(player.seen) < EXPIRY)
            .map(|player| player.addr)
            .collect();
        shared
            .events
            .subscriptions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(addr, _), _| players.contains(addr));
        let mut next = now + RETRY;
        for addr in players {
            for path in SERVICES {
                let sid = {
                    let subscriptions = shared
                        .events
                        .subscriptions
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    match subscriptions.get(&(addr, path)) {
                        Some(subscription) => {
                            if subscription.renew.is_none_or(|renew| renew > clock.now()) {
                                if let Some(renew) = subscription.renew {
                                    next = next.min(renew);
                                }
                                continue;
                            }
                            subscription
                                .expires
                                .is_none_or(|expires| expires > now)
                                .then(|| subscription.sid.clone())
                        }
                        None => None,
                    }
                };
                // Renewal uses only the SID; callback routing is needed for a new lease.
                let mut request = shared
                    .http
                    .request(
                        reqwest::Method::from_bytes(b"SUBSCRIBE").expect("SUBSCRIBE is a method"),
                        format!("http://{addr}{path}"),
                    )
                    .header("TIMEOUT", "Second-300");
                if let Some(sid) = &sid {
                    request = request.header("SID", sid);
                } else {
                    let host = if listen.ip().is_unspecified() {
                        match route_to(addr.ip()) {
                            Ok(host) => host,
                            Err(_) => {
                                tracing::warn!(%addr, "cannot route a Sonos event callback");
                                continue;
                            }
                        }
                    } else {
                        listen.ip()
                    };
                    request = request.header("NT", "upnp:event").header(
                        "CALLBACK",
                        format!(
                            "<http://{}/sonos/events>",
                            SocketAddr::new(host, listen.port())
                        ),
                    );
                }
                // Count connection/response time against the granted lifetime too.
                let began = clock.now();
                let mut invalid_sid = false;
                let mut status = None;
                let granted = match request.send().await {
                    Ok(answer) if answer.status().is_success() => grant(answer.headers()),
                    Ok(answer) => {
                        invalid_sid = answer.status() == reqwest::StatusCode::PRECONDITION_FAILED;
                        status = Some(answer.status().as_u16());
                        Err("HTTP status")
                    }
                    Err(error) => {
                        // Never format reqwest's URL-bearing error: callback SIDs and stream
                        // capabilities must not enter diagnostics.
                        let kind = if error.is_timeout() {
                            "timeout"
                        } else if error.is_connect() {
                            "connect"
                        } else {
                            "request"
                        };
                        Err(kind)
                    }
                };
                if let Ok((new_sid, lifetime)) = granted {
                    let renew = lifetime.map(|lifetime| began + lifetime / 2);
                    let mut subscriptions = shared
                        .events
                        .subscriptions
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    let previous = subscriptions
                        .remove(&(addr, path))
                        .filter(|previous| previous.sid == new_sid);
                    let fresh = previous.is_none();
                    let (sequence, value) = previous
                        .map_or((None, None), |previous| (previous.sequence, previous.value));
                    subscriptions.insert(
                        (addr, path),
                        Subscription {
                            sid: new_sid,
                            sequence,
                            value,
                            expires: lifetime.map(|lifetime| began + lifetime),
                            renew,
                        },
                    );
                    drop(subscriptions);
                    if let Some(renew) = renew {
                        next = next.min(renew);
                    }
                    // Covers an initial snapshot delivered before the SUBSCRIBE response.
                    if fresh {
                        shared.events.wake();
                    }
                } else {
                    // Only a publisher's invalid-SID response revokes a still-valid lease.
                    // Transient failures keep accepting callbacks and retry at RETRY.
                    let mut subscriptions = shared
                        .events
                        .subscriptions
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    let expired = subscriptions.get(&(addr, path)).is_some_and(|lease| {
                        lease.expires.is_some_and(|expiry| expiry <= clock.now())
                    });
                    let dropped = invalid_sid || expired;
                    if dropped {
                        subscriptions.remove(&(addr, path));
                    }
                    let retained = subscriptions.contains_key(&(addr, path));
                    let lease = if retained { "retained" } else { "absent" };
                    let reason = if invalid_sid {
                        "invalid SID"
                    } else if expired {
                        "expired"
                    } else {
                        granted.unwrap_err()
                    };
                    tracing::warn!(
                        renewal = sid.is_some(),
                        status,
                        service = path,
                        lease,
                        dropped,
                        reason,
                        "Sonos subscription failed; retrying"
                    );
                }
            }
        }
        tokio::select! {
            result = searched.changed() => { if result.is_err() { return; } }
            () = clock.sleep_until(next.max(clock.now() + Duration::from_millis(100))) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><ZoneGroupState>topology</ZoneGroupState></e:property></e:propertyset>"#;

    fn subscribed(now: Instant, sequence: Option<u32>) -> Events {
        let events = Events::default();
        events.subscriptions.lock().unwrap().insert(
            ("127.0.0.1:1400".parse().unwrap(), SERVICES[0]),
            Subscription {
                sid: "uuid:current".to_owned(),
                sequence,
                value: None,
                expires: Some(now + Duration::from_secs(60)),
                renew: Some(now + Duration::from_secs(30)),
            },
        );
        events
    }

    #[test]
    fn subscription_debug_redacts_the_sid_and_stream_capability() {
        let events = subscribed(Instant::now(), None);
        events
            .subscriptions
            .lock()
            .unwrap()
            .values_mut()
            .next()
            .unwrap()
            .value = Some("http://bridge/stream/secret-capability/file.mp3".to_owned());
        let rendered = format!("{events:?}");
        assert!(!rendered.contains("uuid:current"));
        assert!(!rendered.contains("secret-capability"));
    }

    #[test]
    fn callbacks_reject_replays_old_sids_expired_leases_and_untrusted_xml() {
        let now = Instant::now();
        let events = subscribed(now, None);
        let changed = events.changed.subscribe();
        assert_eq!(
            events.notify("uuid:old", 0, BODY, now),
            Notification::UnknownSubscription
        );
        assert_eq!(
            events.notify("uuid:current", 0, "<broken>", now),
            Notification::Malformed
        );
        assert_eq!(
            events.notify("uuid:current", 0, "<propertyset/>", now),
            Notification::Malformed
        );
        assert!(!changed.has_changed().unwrap());
        assert_eq!(
            events.notify("uuid:current", 0, BODY, now),
            Notification::Accepted
        );
        assert_eq!(
            events.notify("uuid:current", 3, BODY, now),
            Notification::Accepted
        );
        let changed = events.changed.subscribe();
        assert_eq!(
            events.notify("uuid:current", 3, BODY, now),
            Notification::Ignored
        );
        assert_eq!(
            events.notify("uuid:current", 2, BODY, now),
            Notification::Ignored
        );
        assert_eq!(
            events.notify("uuid:current", 4, BODY, now + Duration::from_secs(60)),
            Notification::UnknownSubscription
        );
        assert!(!changed.has_changed().unwrap());
    }

    #[test]
    fn callback_sequence_wrap_preserves_forward_order() {
        let now = Instant::now();
        let events = subscribed(now, Some(u32::MAX));
        assert_eq!(
            events.notify("uuid:current", 1, BODY, now),
            Notification::Accepted
        );
        assert_eq!(
            events.notify("uuid:current", u32::MAX, BODY, now),
            Notification::Ignored
        );
    }

    #[test]
    fn transport_controls_are_not_source_changes_but_concurrent_uri_changes_are() {
        let now = Instant::now();
        let events = subscribed(now, None);
        let body = |uri, state| {
            format!(
                r#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><LastChange>&lt;Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/"&gt;&lt;InstanceID val="0"&gt;&lt;TransportState val="{state}"/&gt;&lt;AVTransportURI val="{uri}"/&gt;&lt;/InstanceID&gt;&lt;/Event&gt;</LastChange></e:property></e:propertyset>"#
            )
        };
        assert_eq!(
            events.notify("uuid:current", 0, &body("old", "PLAYING"), now),
            Notification::Accepted
        );
        let mut changed = events.changed.subscribe();
        assert_eq!(
            events.notify("uuid:current", 1, &body("old", "PAUSED_PLAYBACK"), now),
            Notification::Accepted
        );
        assert!(!changed.has_changed().unwrap());
        assert_eq!(
            events.notify("uuid:current", 2, &body("external", "PLAYING"), now),
            Notification::Accepted
        );
        assert!(changed.has_changed().unwrap());
        changed.borrow_and_update();
        assert_eq!(
            events.notify("uuid:current", 1, &body("old", "PAUSED_PLAYBACK"), now),
            Notification::Ignored
        );
        assert!(!changed.has_changed().unwrap());
    }

    #[test]
    fn infinite_grant_keeps_a_live_subscription_without_a_renewal_deadline() {
        let now = Instant::now();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("SID", "uuid:current".parse().unwrap());
        headers.insert("TIMEOUT", "Second-infinite".parse().unwrap());
        let (sid, lifetime) = grant(&headers).unwrap();
        let events = Events::default();
        events.subscriptions.lock().unwrap().insert(
            ("127.0.0.1:1400".parse().unwrap(), SERVICES[0]),
            Subscription {
                sid,
                sequence: None,
                value: None,
                expires: lifetime.map(|time| now + time),
                renew: lifetime.map(|time| now + time / 2),
            },
        );
        assert_eq!(
            events.notify("uuid:current", 0, BODY, now + Duration::from_secs(86400)),
            Notification::Accepted
        );
        headers.insert("TIMEOUT", "Second-0".parse().unwrap());
        assert_eq!(grant(&headers).unwrap_err(), "zero TIMEOUT");
    }
}
