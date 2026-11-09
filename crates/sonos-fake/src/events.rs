//! Real HTTP GENA leases: SystemClock by default, or an explicitly supplied event clock.

use std::borrow::Cow;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};

use super::{Served, Shared, escape};

#[derive(Debug)]
struct Lease {
    callback: String,
    player: String,
    topology: bool,
    expires: Instant,
    seq: u32,
}

#[derive(Debug)]
pub(super) struct StateData {
    leases: BTreeMap<String, Lease>,
    next: u64,
    pub(super) timeout: u32,
    pub(super) renewals: usize,
    pub(super) reject: bool,
    pub(super) drop_notifications: bool,
    pub(super) clock: Arc<dyn clock::Clock>,
}

#[derive(Debug)]
pub(super) struct Events(Mutex<StateData>);

impl Default for Events {
    fn default() -> Self {
        Self(Mutex::new(StateData {
            leases: BTreeMap::new(),
            next: 0,
            timeout: 60,
            renewals: 0,
            reject: false,
            drop_notifications: false,
            clock: Arc::new(clock::SystemClock),
        }))
    }
}

impl Events {
    pub(super) fn lock(&self) -> MutexGuard<'_, StateData> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn subscriptions(&self) -> Vec<(String, u32)> {
        self.lock()
            .leases
            .iter()
            .map(|(sid, lease)| (sid.clone(), lease.seq))
            .collect()
    }
}

pub(super) async fn subscribe(
    State(served): State<Served>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
) -> Response {
    if method.as_str() != "SUBSCRIBE" {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let mut events = served.shared.events.lock();
    let now = events.clock.now();
    events.leases.retain(|_, lease| lease.expires > now);
    if events.reject {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let timeout = events.timeout;
    let expires = now + Duration::from_secs(u64::from(timeout));
    let fresh = !headers.contains_key("sid");
    let sid = if let Some(sid) = headers.get("sid").and_then(|value| value.to_str().ok()) {
        let Some(lease) = events.leases.get_mut(sid) else {
            return StatusCode::PRECONDITION_FAILED.into_response();
        };
        if lease.player != served.uuid || lease.topology != uri.path().contains("ZoneGroupTopology")
        {
            return StatusCode::PRECONDITION_FAILED.into_response();
        }
        lease.expires = expires;
        events.renewals += 1;
        sid.to_owned()
    } else {
        if headers.get("nt").and_then(|value| value.to_str().ok()) != Some("upnp:event") {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let Some(callback) = headers
            .get("callback")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix('<'))
            .and_then(|value| value.strip_suffix('>'))
        else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        if !callback.starts_with("http://127.0.0.1:") {
            return StatusCode::BAD_REQUEST.into_response();
        }
        events.next += 1;
        let sid = format!("uuid:fake-{}", events.next);
        events.leases.insert(
            sid.clone(),
            Lease {
                callback: callback.to_owned(),
                player: served.uuid,
                topology: uri.path().contains("ZoneGroupTopology"),
                expires,
                seq: 0,
            },
        );
        sid
    };
    drop(events);
    if fresh {
        tokio::spawn(publish(Arc::clone(&served.shared)));
    }
    (
        StatusCode::OK,
        [("SID", sid), ("TIMEOUT", format!("Second-{timeout}"))],
    )
        .into_response()
}

pub(super) async fn publish(shared: Arc<Shared>) {
    let now = shared.events.lock().clock.now();
    if shared.events.lock().drop_notifications {
        return;
    }
    let messages: Vec<_> = {
        let state = shared.lock();
        let mut events = shared.events.lock();
        events.leases.retain(|_, lease| lease.expires > now);
        let mut topology = None;
        events.leases.iter_mut().filter_map(|(sid, lease)| {
            let property = if lease.topology {
                Cow::Borrowed(topology.get_or_insert_with(|| format!("<ZoneGroupState>{}</ZoneGroupState>", escape(&state.zone_group_state()))).as_str())
            } else {
                let player = state.find(&lease.player)?;
                let transport = &player.transport;
                let uri = transport.track.as_ref().map(|track| track.uri.as_str()).unwrap_or_default();
                let change = format!("<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\"><InstanceID val=\"0\"><TransportState val=\"{}\"/><AVTransportURI val=\"{}\"/></InstanceID></Event>", transport.state, escape(uri));
                Cow::Owned(format!("<LastChange>{}</LastChange>", escape(&change)))
            };
            let seq = lease.seq;
            lease.seq = lease.seq.wrapping_add(1).max(1);
            Some((sid.clone(), seq, lease.callback.clone(), format!("<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\"><e:property>{property}</e:property></e:propertyset>")))
        }).collect()
    };
    for (sid, seq, callback, body) in messages {
        let _ = shared
            .http
            .request(
                Method::from_bytes(b"NOTIFY").expect("NOTIFY method"),
                callback,
            )
            .header("SID", sid)
            .header("SEQ", seq)
            .header("NT", "upnp:event")
            .header("NTS", "upnp:propchange")
            .body(body)
            .send()
            .await;
    }
}
