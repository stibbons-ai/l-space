//! Real SSDP/HTTP leases: stalled peers without leases cannot expire a healthy short lease.
//! The callback uses the same `Sonos::notify` boundary as Bridge's /sonos/events route;
//! Bridge's takeover_after_renewal_preserves_own_controls_and_replacement test
//! separately proves those hints end a Handoff after the original lease expires.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    routing::any,
};
use sonos::{Notification, Sonos};

#[derive(Clone, Default)]
struct Publisher {
    leases: Arc<Mutex<std::collections::BTreeMap<String, Instant>>>,
    requests: Arc<Mutex<Vec<(Instant, bool)>>>,
}

async fn subscribe(
    State(state): State<Publisher>,
    uri: Uri,
    headers: HeaderMap,
) -> (StatusCode, HeaderMap) {
    let now = Instant::now();
    let sid = format!("uuid:healthy-{}", uri.path().replace('/', "-"));
    let mut leases = state.leases.lock().unwrap();
    let renewing = headers.contains_key("sid");
    state.requests.lock().unwrap().push((now, renewing));
    if renewing && (headers["sid"] != sid || leases.get(&sid).is_none_or(|expires| *expires <= now))
    {
        return (StatusCode::PRECONDITION_FAILED, HeaderMap::new());
    }
    leases.insert(sid.clone(), now + Duration::from_secs(2));
    let mut answer = HeaderMap::new();
    answer.insert("sid", sid.parse().unwrap());
    answer.insert("timeout", "Second-2".parse().unwrap());
    (StatusCode::OK, answer)
}

/// Open requests on stalled peers, counted while their handlers run.
struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn stalled(State(in_flight): State<Arc<AtomicUsize>>) -> StatusCode {
    in_flight.fetch_add(1, Ordering::SeqCst);
    let _open = InFlight(in_flight);
    std::future::pending().await
}

/// Polls `done` every 20 ms for at most 10 seconds.
async fn until(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

async fn callback(
    State(upnp): State<Arc<sonos::Upnp>>,
    headers: HeaderMap,
    body: String,
) -> StatusCode {
    match upnp.notify(headers["sid"].to_str().unwrap(), 1, &body) {
        Notification::Accepted => StatusCode::OK,
        _ => StatusCode::PRECONDITION_FAILED,
    }
}

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve(router: Router) -> (std::net::SocketAddr, Server) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (address, Server(server))
}

#[tokio::test]
async fn stalled_peer_does_not_expire_short_leases_or_disable_source_callbacks() {
    short_leases_with_stalled_peers(1).await;
}

#[tokio::test]
async fn saturated_stalled_attempts_yield_to_short_lease_renewals() {
    short_leases_with_stalled_peers(20).await;
}

/// Healthy leases come first; only then are the stalled peers found, by a second search, so
/// their no-lease attempts take as many request slots as they can (all 32 for 20 peers) whatever
/// the ports. Each healthy lease, granted for 2 s and renewed after 1 s, must keep renewing
/// with its SID while those attempts take up to 2 s to time out.
async fn short_leases_with_stalled_peers(stalled_peers: usize) {
    let publisher = Publisher::default();
    let (healthy, _healthy) = serve(
        Router::new()
            .fallback(any(subscribe))
            .with_state(publisher.clone()),
    )
    .await;
    let advertised = Arc::new(Mutex::new(vec![(
        "RINCON_FAKE00000101400".to_owned(),
        healthy,
    )]));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let mut stalled_peers_found = Vec::new();
    let mut servers = Vec::new();
    for index in 0..stalled_peers {
        let router = Router::new()
            .fallback(any(stalled))
            .with_state(Arc::clone(&in_flight));
        let (address, server) = serve(router).await;
        stalled_peers_found.push((format!("RINCON_FAKE{:06}01400", index + 2), address));
        servers.push(server);
    }
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let discovery = socket.local_addr().unwrap();
    let answered = Arc::clone(&advertised);
    let _ssdp = Server(tokio::spawn(async move {
        let mut buffer = [0; 4096];
        loop {
            let (_, from) = socket.recv_from(&mut buffer).await.unwrap();
            let peers = answered.lock().unwrap().clone();
            for (id, address) in peers {
                let response = format!(
                    "HTTP/1.1 200 OK\r\nST: urn:schemas-upnp-org:device:ZonePlayer:1\r\nUSN: uuid:{id}::urn:schemas-upnp-org:device:ZonePlayer:1\r\nLOCATION: http://{address}/xml/device_description.xml\r\n\r\n"
                );
                socket.send_to(response.as_bytes(), from).await.unwrap();
            }
        }
    }));
    // Discovery runs on a manual clock, so the second search comes only when the test says.
    let search_clock = clock::ManualClock::new();
    let upnp = Arc::new(sonos::Upnp::start(
        discovery,
        Arc::new(search_clock.clone()),
    ));
    let (listen, _callback) = serve(
        Router::new()
            .fallback(any(callback))
            .with_state(Arc::clone(&upnp)),
    )
    .await;
    let mut events = upnp.events(listen, Arc::new(clock::SystemClock));
    until("healthy leases were never granted and renewed", || {
        let requests = publisher.requests.lock().unwrap();
        requests.iter().filter(|(_, renewal)| *renewal).count() >= 2
    })
    .await;
    until("discovery never waited for its next search", || {
        search_clock.sleepers() > 0
    })
    .await;
    advertised.lock().unwrap().extend(stalled_peers_found);
    search_clock.advance(sonos::REFRESH);
    let slots = (2 * stalled_peers).min(32);
    until(
        "stalled peers never took every request slot they could",
        || in_flight.load(Ordering::SeqCst) >= slots,
    )
    .await;
    let saturated = Instant::now();
    // Every lease granted before saturation has expired 2 s later; a renewal after that
    // shows the leases were renewed in time rather than recreated.
    until("healthy leases were not renewed through saturation", || {
        let requests = publisher.requests.lock().unwrap();
        requests
            .iter()
            .any(|(at, renewal)| *renewal && *at > saturated + Duration::from_secs(2))
    })
    .await;
    let requests = publisher.requests.lock().unwrap().clone();
    assert_eq!(
        requests.iter().filter(|(_, renewal)| !renewal).count(),
        2,
        "leases expired and had to be recreated"
    );
    // A topology (ZoneGroupState) callback after the original grants expired must still be
    // accepted and wake reconciliation, not merely leave a SID in a request header.
    events.borrow_and_update();
    let sid = publisher
        .leases
        .lock()
        .unwrap()
        .keys()
        .next()
        .unwrap()
        .clone();
    let response = reqwest::Client::new()
        .request(reqwest::Method::from_bytes(b"NOTIFY").unwrap(), format!("http://{listen}/sonos/events"))
        .header("sid", sid)
        .body(r#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><ZoneGroupState>topology-change</ZoneGroupState></e:property></e:propertyset>"#)
        .send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(1), events.changed())
        .await
        .unwrap()
        .unwrap();
}
