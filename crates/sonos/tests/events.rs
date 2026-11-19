//! Real SSDP/HTTP leases: stalled peers, without leases or with lapsed ones, cannot expire a
//! healthy short lease, nor can one transient renewal failure. The callback uses the same
//! `Sonos::notify` boundary as Bridge's /sonos/events route; Bridge's
//! takeover_after_renewal_preserves_own_controls_and_replacement test separately proves those
//! hints end a Handoff after the original lease expires.
use std::collections::BTreeSet;
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

/// Stalled peers: each request stalls, counted while its handler runs (and, without a `SID`,
/// in `fresh`), unless `lease` grants each endpoint's first request a 1 s lease (its `SID`
/// renewals then stall while it lapses).
#[derive(Clone, Default)]
struct Stalled {
    in_flight: Arc<AtomicUsize>,
    fresh: Arc<AtomicUsize>,
    lease: bool,
    granted: Arc<Mutex<BTreeSet<String>>>,
}

/// Open requests on stalled peers, counted while their handlers run.
struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn stalled(
    State(stalled): State<Stalled>,
    uri: Uri,
    headers: HeaderMap,
) -> (StatusCode, HeaderMap) {
    let endpoint = format!("{}{}", headers["host"].to_str().unwrap(), uri.path());
    if stalled.lease && stalled.granted.lock().unwrap().insert(endpoint.clone()) {
        let mut answer = HeaderMap::new();
        answer.insert("sid", format!("uuid:lapsing-{endpoint}").parse().unwrap());
        answer.insert("timeout", "Second-1".parse().unwrap());
        return (StatusCode::OK, answer);
    }
    if !headers.contains_key("sid") {
        stalled.fresh.fetch_add(1, Ordering::SeqCst);
    }
    stalled.in_flight.fetch_add(1, Ordering::SeqCst);
    let _open = InFlight(Arc::clone(&stalled.in_flight));
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

/// An SSDP responder on loopback answering each search with every player in `advertised`
/// (UUID, address) as it is then.
async fn ssdp(
    advertised: Arc<Mutex<Vec<(String, std::net::SocketAddr)>>>,
) -> (std::net::SocketAddr, Server) {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let discovery = socket.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        loop {
            let (_, from) = socket.recv_from(&mut buffer).await.unwrap();
            let peers = advertised.lock().unwrap().clone();
            for (id, address) in peers {
                let response = format!(
                    "HTTP/1.1 200 OK\r\nST: urn:schemas-upnp-org:device:ZonePlayer:1\r\nUSN: uuid:{id}::urn:schemas-upnp-org:device:ZonePlayer:1\r\nLOCATION: http://{address}/xml/device_description.xml\r\n\r\n"
                );
                socket.send_to(response.as_bytes(), from).await.unwrap();
            }
        }
    });
    (discovery, Server(server))
}

#[tokio::test]
async fn stalled_peer_does_not_expire_short_leases_or_disable_source_callbacks() {
    short_leases_with_stalled_peers(1, false).await;
}

#[tokio::test]
async fn saturated_stalled_attempts_yield_to_short_lease_renewals() {
    short_leases_with_stalled_peers(20, false).await;
}

/// 16 peers, two endpoints each, fill all 32 request slots with `SID` renewals that stall past
/// their 1 s leases: lapsed, those attempts must yield to a healthy lease's renewal.
#[tokio::test]
async fn saturated_lapsed_renewals_yield_to_short_lease_renewals() {
    short_leases_with_stalled_peers(16, true).await;
}

/// Healthy leases come first; only then are the stalled peers found, by a second search, so
/// their attempts take as many request slots as they can (all 32 for 16 or more peers)
/// whatever the ports: without leases, or, `leased`, renewing leases that lapse while they
/// stall. Each healthy lease, granted for 2 s and renewed after 1 s, must keep renewing with
/// its SID while those attempts take up to 2 s to time out.
async fn short_leases_with_stalled_peers(stalled_peers: usize, leased: bool) {
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
    let stalled_state = Stalled {
        lease: leased,
        ..Stalled::default()
    };
    let in_flight = Arc::clone(&stalled_state.in_flight);
    let mut stalled_peers_found = Vec::new();
    let mut servers = Vec::new();
    for index in 0..stalled_peers {
        let router = Router::new()
            .fallback(any(stalled))
            .with_state(stalled_state.clone());
        let (address, server) = serve(router).await;
        stalled_peers_found.push((format!("RINCON_FAKE{:06}01400", index + 2), address));
        servers.push(server);
    }
    let (discovery, _ssdp) = ssdp(Arc::clone(&advertised)).await;
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
    if leased {
        assert_eq!(
            stalled_state.fresh.load(Ordering::SeqCst),
            0,
            "the slots were not all taken by renewals of granted leases"
        );
    }
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

/// A prompt 503 on the first renewal of a 2 s lease leaves about 1 s of it: the retry must
/// come within that, so the same SID keeps delivering past the original expiry.
#[tokio::test]
async fn a_transient_renewal_failure_does_not_expire_a_short_lease() {
    const PATH: &str = "/MediaRenderer/AVTransport/Event";
    let publisher = Publisher::default();
    let failed = Arc::new(AtomicUsize::new(0));
    let fault = Arc::clone(&failed);
    let (player, _player) = serve(
        Router::new()
            .fallback(any(
                move |state: State<Publisher>, uri: Uri, headers: HeaderMap| {
                    let fault = Arc::clone(&fault);
                    async move {
                        if uri.path() == PATH
                            && headers.contains_key("sid")
                            && fault.fetch_add(1, Ordering::SeqCst) == 0
                        {
                            return (StatusCode::SERVICE_UNAVAILABLE, HeaderMap::new());
                        }
                        subscribe(state, uri, headers).await
                    }
                },
            ))
            .with_state(publisher.clone()),
    )
    .await;
    let advertised = vec![("RINCON_FAKE00000101400".to_owned(), player)];
    let (discovery, _ssdp) = ssdp(Arc::new(Mutex::new(advertised))).await;
    let upnp = Arc::new(sonos::Upnp::start(discovery, Arc::new(clock::SystemClock)));
    let (listen, _callback) = serve(
        Router::new()
            .fallback(any(callback))
            .with_state(Arc::clone(&upnp)),
    )
    .await;
    let _events = upnp.events(listen, Arc::new(clock::SystemClock));
    let sid = format!("uuid:healthy-{}", PATH.replace('/', "-"));
    until("transport lease never granted", || {
        publisher.leases.lock().unwrap().contains_key(&sid)
    })
    .await;
    let original_expiry = publisher.leases.lock().unwrap()[&sid];
    until("no 503 on the first transport renewal", || {
        failed.load(Ordering::SeqCst) > 0
    })
    .await;
    assert!(Instant::now() < original_expiry);
    tokio::time::sleep_until((original_expiry + Duration::from_millis(100)).into()).await;
    let response = reqwest::Client::new()
        .request(reqwest::Method::from_bytes(b"NOTIFY").unwrap(), format!("http://{listen}/sonos/events"))
        .header("sid", &sid)
        .body(r#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><ZoneGroupState>change</ZoneGroupState></e:property></e:propertyset>"#)
        .send().await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the original SID stopped delivering after one prompt 503"
    );
}
