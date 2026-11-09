//! GENA subscriptions. Bodies only wake readers: delayed notifications cannot apply old
//! transport or topology to a newer Handoff. SIDs and URLs are never logged.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::{EXPIRY, Shared, route_to};

const SERVICES: [&str; 2] = [
    "/MediaRenderer/AVTransport/Event",
    "/ZoneGroupTopology/Event",
];
const RETRY: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct Subscription {
    sid: String,
    sequence: Option<u32>,
    expires: Instant,
    value: Option<String>,
    renew: Instant,
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

    pub(super) fn notify(&self, sid: &str, seq: u32, body: &str, now: Instant) -> bool {
        if body.len() > crate::MAX_BODY {
            return false;
        }
        let Ok(document) = roxmltree::Document::parse_with_options(
            body,
            roxmltree::ParsingOptions {
                nodes_limit: crate::xml::MAX_NODES,
                ..Default::default()
            },
        ) else {
            return false;
        };
        if !document
            .root_element()
            .has_tag_name(("urn:schemas-upnp-org:event-1-0", "propertyset"))
        {
            return false;
        }
        let mut subscriptions = self
            .subscriptions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(subscription) = subscriptions
            .values_mut()
            .find(|entry| entry.sid == sid && entry.expires > now)
        else {
            return false;
        };
        if subscription.sequence.is_some_and(|previous| {
            let distance = seq.wrapping_sub(previous);
            distance == 0 || distance >= (1 << 31)
        }) {
            return false;
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
        true
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
    let change = roxmltree::Document::parse_with_options(
        change,
        roxmltree::ParsingOptions {
            nodes_limit: crate::xml::MAX_NODES,
            ..Default::default()
        },
    )
    .ok()?;
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
                            if subscription.renew > clock.now() {
                                next = next.min(subscription.renew);
                                continue;
                            }
                            (subscription.expires > now).then(|| subscription.sid.clone())
                        }
                        None => None,
                    }
                };
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
                let answer = request.send().await;
                let granted = answer
                    .ok()
                    .filter(|answer| answer.status().is_success())
                    .and_then(|answer| {
                        let sid = answer.headers().get("SID")?.to_str().ok()?.to_owned();
                        if sid.is_empty() || sid.len() > 256 {
                            return None;
                        }
                        let timeout = answer.headers().get("TIMEOUT")?.to_str().ok()?;
                        let seconds = timeout.strip_prefix("Second-")?.parse::<u32>().ok()?;
                        (seconds > 0).then_some((sid, Duration::from_secs(u64::from(seconds))))
                    });
                if let Some((new_sid, lifetime)) = granted {
                    let renew = began + lifetime / 2;
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
                            expires: began + lifetime,
                            renew,
                        },
                    );
                    drop(subscriptions);
                    next = next.min(renew);
                    // Covers an initial snapshot delivered before the SUBSCRIBE response.
                    if fresh {
                        shared.events.wake();
                    }
                } else {
                    tracing::warn!(%addr, service = path, "Sonos event subscription failed; retrying");
                    // A refused SID must not be renewed forever: establish a new subscription.
                    shared
                        .events
                        .subscriptions
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&(addr, path));
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
                expires: now + Duration::from_secs(60),
                renew: now + Duration::from_secs(30),
            },
        );
        events
    }

    #[test]
    fn callbacks_reject_replays_old_sids_expired_leases_and_untrusted_xml() {
        let now = Instant::now();
        let events = subscribed(now, None);
        let changed = events.changed.subscribe();
        assert!(!events.notify("uuid:old", 0, BODY, now));
        assert!(!events.notify("uuid:current", 0, "<broken>", now));
        assert!(!events.notify("uuid:current", 0, "<propertyset/>", now));
        assert!(!changed.has_changed().unwrap());
        assert!(events.notify("uuid:current", 0, BODY, now));
        assert!(events.notify("uuid:current", 3, BODY, now));
        let changed = events.changed.subscribe();
        assert!(!events.notify("uuid:current", 3, BODY, now));
        assert!(!events.notify("uuid:current", 2, BODY, now));
        assert!(!events.notify("uuid:current", 4, BODY, now + Duration::from_secs(60)));
        assert!(!changed.has_changed().unwrap());
    }

    #[test]
    fn callback_sequence_wrap_preserves_forward_order() {
        let now = Instant::now();
        let events = subscribed(now, Some(u32::MAX));
        assert!(events.notify("uuid:current", 1, BODY, now));
        assert!(!events.notify("uuid:current", u32::MAX, BODY, now));
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
        assert!(events.notify("uuid:current", 0, &body("old", "PLAYING"), now));
        let mut changed = events.changed.subscribe();
        assert!(events.notify("uuid:current", 1, &body("old", "PAUSED_PLAYBACK"), now));
        assert!(!changed.has_changed().unwrap());
        assert!(events.notify("uuid:current", 2, &body("external", "PLAYING"), now));
        assert!(changed.has_changed().unwrap());
        changed.borrow_and_update();
        assert!(!events.notify("uuid:current", 1, &body("old", "PAUSED_PLAYBACK"), now));
        assert!(!changed.has_changed().unwrap());
    }
}
