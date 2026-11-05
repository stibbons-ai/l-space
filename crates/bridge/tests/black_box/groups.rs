//! The household's Sonos Groups, read from `sonos-fake` players on loopback: every Group
//! exactly as grouped in Sonos with what it plays, regrouping, positions moving on the shared
//! clock, misbehaving players, and the raw UPnP traffic in the logs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use api::{Group, GroupMember, NowPlaying, Session, TransportState};
use clock::ManualClock;
use reqwest::StatusCode;
use serde_json::{Value, json};
use sonos_fake::{FakeTrack, Fault, Household, PlayerId};

use test_support::audiobookshelf::Audiobookshelf;
use test_support::bridge::{
    RUST_LOG, SESSION_INVALID, TestBridge, named_library_server_toml, output_contains, sonos_toml,
};

/// A household on a manual clock.
fn new_household() -> (Household, ManualClock) {
    let clock = ManualClock::new();
    (Household::start(Arc::new(clock.clone())), clock)
}

/// A Bridge searching `household` for players, logging with `rust_log`, and a session on it.
async fn logged_in_logging(household: &Household, rust_log: &str) -> (TestBridge, Session) {
    let abs = Audiobookshelf::shared();
    let config = format!(
        "{}{}",
        named_library_server_toml(abs.base_url(), "Home"),
        sonos_toml(household.ssdp())
    );
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

#[tokio::test]
async fn every_group_is_listed_as_grouped_in_sonos_with_what_it_plays() {
    let (household, _) = new_household();
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
            ]
            .contains(&action.as_str()),
            "{action}"
        );
    }
}

/// Pins the wire shape the App decodes, independent of the shared Rust types.
#[tokio::test]
async fn group_list_wire_shape() {
    let (household, _) = new_household();
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
            },
            {
                "id": office.uuid(),
                "name": "Office",
                "members": [{"id": office.uuid(), "name": "Office"}],
                "state": "stopped",
                "now_playing": null,
            },
        ])
    );
}

#[tokio::test]
async fn regrouping_in_sonos_shows_in_the_next_listing() {
    let (household, _) = new_household();
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
async fn a_playing_groups_position_moves_with_the_clock_it_shares_with_the_fake() {
    let (household, clock) = new_household();
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

    // Never past the end of the track.
    household.play(&kitchen, FakeTrack::new("Short", Duration::from_secs(30)));
    clock.advance(Duration::from_secs(45));
    assert_eq!(position().await, Some(30.0));
}

#[tokio::test]
async fn a_household_without_players_has_no_groups() {
    let (household, _) = new_household();
    let (bridge, session) = logged_in(&household).await;

    assert_eq!(bridge.api().groups(&session).await, []);
}

#[tokio::test]
async fn groups_need_a_session() {
    let (household, _) = new_household();
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
    let (household, _) = new_household();
    // Asked first for the topology: players are tried in UUID order.
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
async fn players_that_answer_garbage_or_never_answer_are_reported_in_bounded_time() {
    let sonos_unreachable = (StatusCode::BAD_GATEWAY, api::ApiError::SonosUnreachable);
    for fault in [Fault::Garbage, Fault::Hang] {
        let (household, _) = new_household();
        let kitchen = household.add_player("Kitchen");
        household.set_fault(&kitchen, fault);
        let (bridge, session) = logged_in(&household).await;
        let api = bridge.api();

        let start = Instant::now();
        assert_eq!(
            api.get_as_error(&session, "/api/groups", &[]).await,
            sonos_unreachable,
            "{fault:?}"
        );
        // Well within the App's 15-second limit for a call.
        assert!(start.elapsed() < Duration::from_secs(10), "{fault:?}");

        // The Bridge is unharmed.
        assert_eq!(api.library_servers().await.len(), 1);
    }
}

#[tokio::test]
async fn raw_upnp_traffic_is_logged_at_debug_only() {
    let raw = [
        "M-SEARCH",
        "ssdp:discover",
        "s:Envelope",
        "GetZoneGroupStateResponse",
    ];

    // At the default level, none of it shows, at any level.
    let (household, _) = new_household();
    household.add_player("Kitchen");
    let (bridge, session) = logged_in(&household).await;
    assert_eq!(bridge.api().groups(&session).await.len(), 1);
    let output = bridge.stop();
    for text in raw {
        assert!(!output_contains(&output, text), "{text}: {output:#?}");
    }

    // With the raw target at debug, every SSDP datagram and SOAP body shows there, at DEBUG.
    let (household, _) = new_household();
    household.add_player("Kitchen");
    let (bridge, session) = logged_in_logging(&household, "sonos::raw=debug").await;
    assert_eq!(bridge.api().groups(&session).await.len(), 1);
    let output = bridge.stop();
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
