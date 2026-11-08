//! The household's Sonos Groups, read from `sonos-fake` players on loopback: every Group
//! exactly as grouped in Sonos with what it plays, regrouping, positions moving on the fake's
//! clock, misbehaving players, failed searches, and the raw UPnP traffic in the logs. The
//! Bridge runs on the system clock; that its search timing follows the injected clock is
//! `crates/sonos/tests/discovery.rs`'s.

use std::time::{Duration, Instant};

use api::{Group, GroupMember, NowPlaying, Session, TransportState};
use serde_json::{Value, json};
use sonos_fake::{FakeTrack, Fault, Household, PlayerId};

use test_support::audiobookshelf::Audiobookshelf;
use test_support::bridge::{
    RUST_LOG, SESSION_INVALID, SONOS_SEARCH_FAILED, SONOS_UNREACHABLE, TestBridge,
    home_with_sonos_toml, output_contains,
};

/// A Bridge searching `household` for players, logging with `rust_log`, and a session on it.
async fn logged_in_logging(household: &Household, rust_log: &str) -> (TestBridge, Session) {
    let abs = Audiobookshelf::shared();
    let config = home_with_sonos_toml(abs.base_url(), household.ssdp());
    let bridge = TestBridge::start_logging(&config, rust_log);
    let user = abs.new_user().await;
    let session = bridge.api().login(abs.base_url(), &user.api_key.key).await;
    (bridge, session)
}

async fn logged_in(household: &Household) -> (TestBridge, Session) {
    logged_in_logging(household, RUST_LOG).await
}

fn member(player: &PlayerId, name: &str) -> GroupMember {
    GroupMember {
        id: player.uuid().to_owned(),
        name: name.to_owned(),
    }
}

/// The Groups' names, each with its members' names.
fn names(groups: &[Group]) -> Vec<(String, Vec<String>)> {
    groups
        .iter()
        .map(|group| {
            let members = group.members.iter().map(|member| member.name.clone());
            (group.name.clone(), members.collect())
        })
        .collect()
}

fn named(groups: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
    groups
        .iter()
        .map(|(name, members)| {
            let members = members.iter().map(|member| member.to_string());
            (name.to_string(), members.collect())
        })
        .collect()
}

const TEN_MINUTES: Duration = Duration::from_secs(600);

/// The Bridge's limit for a Groups listing, plus a second for the call around it; well within
/// the App's 15-second limit for a call.
const LISTING_BOUND: Duration = sonos::LISTING_TIMEOUT.saturating_add(Duration::from_secs(1));

#[tokio::test]
async fn every_group_is_listed_as_grouped_in_sonos_with_what_it_plays() {
    let (household, _) = Household::with_manual_clock();
    // A two-player Group, a speaker on its own, and a Beam with a bonded surround.
    let kitchen = household.add_player("Kitchen");
    let bath = household.add_player("Bath");
    household.join(&bath, &kitchen);
    let office = household.add_player("Office");
    let beam = household.add_player("Daniels Zimmer");
    household.add_bonded(&beam);
    household.play(
        &kitchen,
        FakeTrack::new("Moby Dick", TEN_MINUTES).by("Herman Melville"),
    );
    household.play(&office, FakeTrack::new("Silence", TEN_MINUTES));
    household.pause(&office);
    let (bridge, session) = logged_in(&household).await;

    assert_eq!(
        bridge.api().groups(&session).await,
        [
            Group {
                id: beam.uuid().to_owned(),
                name: "Daniels Zimmer".into(),
                // The bonded surround is not a member.
                members: vec![member(&beam, "Daniels Zimmer")],
                state: Some(TransportState::Stopped),
                now_playing: None,
                volume: Some(sonos_fake::DEFAULT_VOLUME),
                handoff: None,
            },
            Group {
                id: kitchen.uuid().to_owned(),
                name: "Kitchen".into(),
                members: vec![member(&kitchen, "Kitchen"), member(&bath, "Bath")],
                state: Some(TransportState::Playing),
                now_playing: Some(NowPlaying {
                    title: Some("Moby Dick".into()),
                    creator: Some("Herman Melville".into()),
                    position: Some(0.0),
                    duration: Some(600.0),
                }),
                volume: Some(sonos_fake::DEFAULT_VOLUME),
                handoff: None,
            },
            Group {
                id: office.uuid().to_owned(),
                name: "Office".into(),
                members: vec![member(&office, "Office")],
                state: Some(TransportState::Paused),
                now_playing: Some(NowPlaying {
                    title: Some("Silence".into()),
                    creator: None,
                    position: Some(0.0),
                    duration: Some(600.0),
                }),
                volume: Some(sonos_fake::DEFAULT_VOLUME),
                handoff: None,
            },
        ]
    );
    // Only reads: nothing that changes playback, volume or grouping.
    for action in household.actions() {
        assert!(
            [
                "ZoneGroupTopology#GetZoneGroupState",
                "AVTransport#GetTransportInfo",
                "AVTransport#GetPositionInfo",
                "GroupRenderingControl#GetGroupVolume",
            ]
            .contains(&action.as_str()),
            "{action}"
        );
    }
}

/// Pins the wire shape the App decodes, independent of the shared Rust types.
#[tokio::test]
async fn group_list_wire_shape() {
    let (household, _) = Household::with_manual_clock();
    let kitchen = household.add_player("Kitchen");
    household.play(
        &kitchen,
        FakeTrack::new("Moby Dick", TEN_MINUTES).by("Herman Melville"),
    );
    let office = household.add_player("Office");
    let (bridge, session) = logged_in(&household).await;

    let body: Value = bridge.api().get_as(&session, "/api/groups", &[]).await;
    assert_eq!(
        body,
        json!([
            {
                "id": kitchen.uuid(),
                "name": "Kitchen",
                "members": [{"id": kitchen.uuid(), "name": "Kitchen"}],
                "state": "playing",
                "now_playing": {
                    "title": "Moby Dick",
                    "creator": "Herman Melville",
                    "position": 0.0,
                    "duration": 600.0,
                },
                "volume": 20,
                "handoff": null,
            },
            {
                "id": office.uuid(),
                "name": "Office",
                "members": [{"id": office.uuid(), "name": "Office"}],
                "state": "stopped",
                "now_playing": null,
                "volume": 20,
                "handoff": null,
            },
        ])
    );
}

#[tokio::test]
async fn regrouping_in_sonos_shows_in_the_next_listing() {
    let (household, _) = Household::with_manual_clock();
    let kitchen = household.add_player("Kitchen");
    let bath = household.add_player("Bath");
    let (bridge, session) = logged_in(&household).await;
    let api = bridge.api();

    let apart = named(&[("Bath", &["Bath"]), ("Kitchen", &["Kitchen"])]);
    assert_eq!(names(&api.groups(&session).await), apart);

    household.join(&bath, &kitchen);
    assert_eq!(
        names(&api.groups(&session).await),
        named(&[("Kitchen", &["Kitchen", "Bath"])])
    );

    household.leave(&bath);
    assert_eq!(names(&api.groups(&session).await), apart);
}

#[tokio::test]
async fn the_coordinator_is_listed_first_whatever_the_topology_order() {
    let (household, _) = Household::with_manual_clock();
    // The fake lists members in the order they were added: Bath before its coordinator.
    let bath = household.add_player("Bath");
    let kitchen = household.add_player("Kitchen");
    household.join(&bath, &kitchen);
    let (bridge, session) = logged_in(&household).await;

    assert_eq!(
        names(&bridge.api().groups(&session).await),
        named(&[("Kitchen", &["Kitchen", "Bath"])])
    );
}

#[tokio::test]
async fn a_playing_groups_position_moves_with_the_fakes_clock() {
    let (household, clock) = Household::with_manual_clock();
    let kitchen = household.add_player("Kitchen");
    household.play(&kitchen, FakeTrack::new("Moby Dick", TEN_MINUTES));
    let (bridge, session) = logged_in(&household).await;
    let position = async || {
        let groups = bridge.api().groups(&session).await;
        groups[0]
            .now_playing
            .as_ref()
            .and_then(|track| track.position)
    };

    assert_eq!(position().await, Some(0.0));
    clock.advance(Duration::from_secs(90));
    assert_eq!(position().await, Some(90.0));

    // Paused, it stays put however much time passes.
    household.pause(&kitchen);
    clock.advance(Duration::from_secs(60));
    assert_eq!(position().await, Some(90.0));

    // Never past the end of the track: played to its end, it stops, back at its start.
    household.play(&kitchen, FakeTrack::new("Short", Duration::from_secs(30)));
    clock.advance(Duration::from_secs(29));
    assert_eq!(position().await, Some(29.0));
    clock.advance(Duration::from_secs(16));
    assert_eq!(position().await, Some(0.0));
    let groups = bridge.api().groups(&session).await;
    assert_eq!(groups[0].state, Some(TransportState::Stopped));
}

#[tokio::test]
async fn a_household_without_players_has_no_groups() {
    let (household, _) = Household::with_manual_clock();
    let (bridge, session) = logged_in(&household).await;

    assert_eq!(bridge.api().groups(&session).await, []);
}

#[tokio::test]
async fn groups_need_a_session() {
    let (household, _) = Household::with_manual_clock();
    household.add_player("Kitchen");
    let (bridge, session) = logged_in(&household).await;
    let api = bridge.api();

    assert_eq!(
        api.get_error("/api/groups", &[], None).await,
        SESSION_INVALID
    );
    api.logout(&session).await;
    assert_eq!(
        api.get_as_error(&session, "/api/groups", &[]).await,
        SESSION_INVALID
    );
}

#[tokio::test]
async fn a_player_answering_garbage_is_passed_over_for_the_topology_and_has_no_playback() {
    let (household, _) = Household::with_manual_clock();
    // Asked first for the topology: at first, players are asked in UUID order.
    let kitchen = household.add_player("Kitchen");
    let bath = household.add_player("Bath");
    household.play(&bath, FakeTrack::new("Moby Dick", TEN_MINUTES));
    household.set_fault(&kitchen, Fault::Garbage);
    let (bridge, session) = logged_in(&household).await;

    let groups = bridge.api().groups(&session).await;
    assert_eq!(
        names(&groups),
        named(&[("Bath", &["Bath"]), ("Kitchen", &["Kitchen"])])
    );
    assert_eq!(groups[0].state, Some(TransportState::Playing));
    assert_eq!((groups[1].state, &groups[1].now_playing), (None, &None));
}

#[tokio::test]
async fn players_that_misbehave_are_reported_in_bounded_time() {
    for fault in [
        Fault::Garbage,
        Fault::Hang,
        // Valid answers, but past what the Bridge reads.
        Fault::Padded(sonos::MAX_BODY + 1),
    ] {
        let (household, _) = Household::with_manual_clock();
        let kitchen = household.add_player("Kitchen");
        household.set_fault(&kitchen, fault);
        let (bridge, session) = logged_in(&household).await;
        let api = bridge.api();

        let start = Instant::now();
        assert_eq!(
            api.get_as_error(&session, "/api/groups", &[]).await,
            SONOS_UNREACHABLE,
            "{fault:?}"
        );
        assert!(start.elapsed() < LISTING_BOUND, "{fault:?}");

        // The Bridge is unharmed.
        assert_eq!(api.library_servers().await.len(), 1);
    }
}

#[tokio::test]
async fn an_answer_as_long_as_the_bridge_reads_is_read() {
    let (household, _) = Household::with_manual_clock();
    let kitchen = household.add_player("Kitchen");
    household.set_fault(&kitchen, Fault::Padded(sonos::MAX_BODY));
    let (bridge, session) = logged_in(&household).await;

    let groups = bridge.api().groups(&session).await;
    assert_eq!(names(&groups), named(&[("Kitchen", &["Kitchen"])]));
    assert_eq!(groups[0].state, Some(TransportState::Stopped));
}

#[tokio::test]
async fn players_that_never_answer_hold_up_the_listing_only_so_long() {
    let (household, _) = Household::with_manual_clock();
    // Added first, so asked first for the topology.
    for n in 0..5 {
        let silent = household.add_player(&format!("Silent {n}"));
        household.set_fault(&silent, Fault::Hang);
    }
    let kitchen = household.add_player("Kitchen");
    let (bridge, session) = logged_in(&household).await;
    let api = bridge.api();

    let start = Instant::now();
    let groups = api.groups(&session).await;
    assert!(start.elapsed() < LISTING_BOUND, "{:?}", start.elapsed());
    // Every Group is listed; those whose coordinator never answers without playback.
    assert_eq!(groups.len(), 6);
    for group in &groups {
        let answered = group.name == "Kitchen";
        assert_eq!(group.state.is_some(), answered, "{group:?}");
    }

    // With only silent players, the listing gives up within its bound.
    household.set_fault(&kitchen, Fault::Hang);
    let start = Instant::now();
    assert_eq!(
        api.get_as_error(&session, "/api/groups", &[]).await,
        SONOS_UNREACHABLE
    );
    assert!(start.elapsed() < LISTING_BOUND, "{:?}", start.elapsed());
    assert_eq!(api.library_servers().await.len(), 1);
}

#[tokio::test]
async fn a_coordinator_no_search_found_is_never_asked_for_its_playback() {
    let (household, _) = Household::with_manual_clock();
    let kitchen = household.add_player("Kitchen");
    let (bridge, session) = logged_in(&household).await;
    // The first search is over (a listing waits for it), and the next is a minute away on the
    // Bridge's own (system) clock: Bath is never announced to it.
    assert_eq!(bridge.api().groups(&session).await.len(), 1);
    let bath = household.add_player("Bath");
    household.play(&bath, FakeTrack::new("Moby Dick", TEN_MINUTES));
    household.join(&kitchen, &bath);

    let groups = bridge.api().groups(&session).await;
    assert_eq!(names(&groups), named(&[("Bath", &["Bath", "Kitchen"])]));
    assert_eq!((groups[0].state, &groups[0].now_playing), (None, &None));
    assert_eq!(household.actions_of(&bath), Vec::<String>::new());
}

#[tokio::test]
async fn a_failed_search_is_reported_rather_than_an_empty_household() {
    let abs = Audiobookshelf::shared();
    // Port 0 can't be sent to: both macOS and Linux refuse it before routing, so nothing
    // leaves loopback.
    let unsendable = "127.0.0.1:0".parse().unwrap();
    let bridge = TestBridge::start(&home_with_sonos_toml(abs.base_url(), unsendable));
    let user = abs.new_user().await;
    let session = bridge.api().login(abs.base_url(), &user.api_key.key).await;

    let start = Instant::now();
    assert_eq!(
        bridge
            .api()
            .get_as_error(&session, "/api/groups", &[])
            .await,
        SONOS_SEARCH_FAILED
    );
    // Answered at once, not when the listing's time is up.
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn the_player_that_told_the_topology_is_asked_first_next_time() {
    let (household, _) = Household::with_manual_clock();
    // Added first, so asked first on the first listing.
    let silent = household.add_player("Silent");
    household.set_fault(&silent, Fault::Hang);
    let kitchen = household.add_player("Kitchen");
    let (bridge, session) = logged_in(&household).await;
    let asked_for_topology = |player| {
        let actions = household.actions_of(player);
        actions
            .iter()
            .filter(|action| *action == "ZoneGroupTopology#GetZoneGroupState")
            .count()
    };

    assert_eq!(bridge.api().groups(&session).await.len(), 2);
    assert_eq!(asked_for_topology(&silent), 1);
    assert_eq!(asked_for_topology(&kitchen), 1);

    // Kitchen told it, and answers at once (within the stagger), so the silent player isn't
    // asked again.
    assert_eq!(bridge.api().groups(&session).await.len(), 2);
    assert_eq!(asked_for_topology(&silent), 1);
    assert_eq!(asked_for_topology(&kitchen), 2);
}

#[tokio::test]
async fn an_unknown_transport_state_is_logged_with_its_group() {
    let (household, _) = Household::with_manual_clock();
    let office = household.add_player("Office");
    household.set_transport_state(&office, "RECORDING");
    let (bridge, session) = logged_in(&household).await;

    let groups = bridge.api().groups(&session).await;
    assert_eq!(groups[0].state, Some(TransportState::Unknown));
    let output = bridge.stop();
    assert!(
        output.iter().any(|event| event["level"] == "WARN"
            && event["message"] == "unknown Sonos transport state"
            && event["group"] == office.uuid()
            && event["state"] == "RECORDING"),
        "{output:#?}"
    );
}

#[tokio::test]
async fn raw_upnp_traffic_is_logged_at_debug_only() {
    let raw = [
        "M-SEARCH",
        "ssdp:discover",
        "s:Envelope",
        "GetZoneGroupStateResponse",
    ];

    // At the default level, none of it shows in any event.
    let (household, _) = Household::with_manual_clock();
    household.add_player("Kitchen");
    let (bridge, session) = logged_in(&household).await;
    assert_eq!(bridge.api().groups(&session).await.len(), 1);
    let output = bridge.stop();
    for text in raw {
        assert!(!output_contains(&output, text), "{text}: {output:#?}");
    }

    // With the raw target at debug, every SSDP datagram and SOAP body shows there, at DEBUG.
    let (household, _) = Household::with_manual_clock();
    household.add_player("Kitchen");
    let (bridge, session) = logged_in_logging(&household, "sonos::raw=debug").await;
    assert_eq!(bridge.api().groups(&session).await.len(), 1);
    let output = bridge.stop();
    // The request the Bridge sent, on its own.
    assert!(
        output.iter().any(|event| event["target"] == sonos::RAW
            && event["message"] == "SOAP request"
            && event["body"]
                .as_str()
                .is_some_and(|body| body.contains("<u:GetZoneGroupState "))),
        "{output:#?}"
    );
    let traffic: Vec<&Value> = output
        .iter()
        .filter(|event| event["target"] == sonos::RAW)
        .collect();
    assert!(
        traffic.iter().all(|event| event["level"] == "DEBUG"),
        "{traffic:#?}"
    );
    let raw_fields: Vec<&str> = traffic
        .iter()
        .flat_map(|event| [&event["datagram"], &event["body"]])
        .filter_map(Value::as_str)
        .collect();
    for text in raw {
        assert!(
            raw_fields.iter().any(|field| field.contains(text)),
            "{text}: {traffic:#?}"
        );
    }
    // A player's answer, as received.
    assert!(
        raw_fields
            .iter()
            .any(|field| field.contains("USN: uuid:RINCON_FAKE")),
        "{traffic:#?}"
    );
}
