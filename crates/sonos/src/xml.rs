//! What players answer, read from XML. Every byte comes from the LAN, so it is untrusted: a
//! document that doesn't parse, or lacks what is looked for, is an error or an absent value,
//! never a panic. DTDs are refused (roxmltree's default) and documents are capped at
//! [`MAX_NODES`] nodes.

use std::time::Duration;

use roxmltree::{Document, Node, ParsingOptions};

use crate::TransportState;

/// More than a household of dozens of players needs.
const MAX_NODES: u32 = 100_000;

/// Dublin Core, which DIDL-Lite uses for an item's title and creator.
const DC: &str = "http://purl.org/dc/elements/1.1/";

/// Why a player's answer could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Malformed {
    #[error("not XML: {0}")]
    Xml(String),
    #[error("no {0} element")]
    Missing(&'static str),
}

fn parse(text: &str) -> Result<Document<'_>, Malformed> {
    let options = ParsingOptions {
        nodes_limit: MAX_NODES,
        ..ParsingOptions::default()
    };
    Document::parse_with_options(text, options).map_err(|error| Malformed::Xml(error.to_string()))
}

/// The output arguments of a SOAP action's answer, `<u:{action}Response>`'s children, by name
/// and text. A SOAP fault (or anything else) has no such element.
pub(crate) fn soap_response(body: &str, action: &str) -> Result<Vec<(String, String)>, Malformed> {
    let document = parse(body)?;
    let name = format!("{action}Response");
    let response = document
        .descendants()
        .find(|node| node.is_element() && node.tag_name().name() == name)
        .ok_or(Malformed::Missing("action response"))?;
    Ok(response
        .children()
        .filter(Node::is_element)
        .map(|argument| {
            let text = argument.text().unwrap_or_default().to_owned();
            (argument.tag_name().name().to_owned(), text)
        })
        .collect())
}

/// One Group as ZoneGroupTopology describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ZoneGroup {
    /// The coordinator's UUID.
    pub coordinator: String,
    /// In document order.
    pub members: Vec<ZoneMember>,
}

/// A `ZoneGroupMember`: a player in a Group, visible in the Sonos app or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ZoneMember {
    pub uuid: String,
    pub name: String,
    /// Its device description URL.
    pub location: Option<String>,
    /// Shown in the Sonos app as (part of) a room. Bonded surrounds and subs, the second
    /// speaker of a stereo pair (`Invisible="1"`) and Boosts and Bridges (`IsZoneBridge="1"`)
    /// are not.
    pub visible: bool,
}

/// The Groups in a `ZoneGroupState` value: the document `GetZoneGroupState` returns escaped
/// inside its answer, rooted at `ZoneGroupState` (current firmware) or `ZoneGroups` (older).
/// A `ZoneGroup` without a coordinator, and a member without a UUID, are left out.
/// `Satellite` elements (bonded home theatre speakers on current firmware) are not members.
pub(crate) fn zone_groups(state: &str) -> Result<Vec<ZoneGroup>, Malformed> {
    let document = parse(state)?;
    let groups_root = document
        .descendants()
        .find(|node| node.has_tag_name("ZoneGroups"))
        .ok_or(Malformed::Missing("ZoneGroups"))?;
    Ok(groups_root
        .children()
        .filter(|node| node.has_tag_name("ZoneGroup"))
        .filter_map(|group| {
            let coordinator = non_empty(group.attribute("Coordinator"))?;
            let members = group
                .children()
                .filter(|node| node.has_tag_name("ZoneGroupMember"))
                .filter_map(zone_member)
                .collect();
            Some(ZoneGroup {
                coordinator,
                members,
            })
        })
        .collect())
}

fn zone_member(member: Node) -> Option<ZoneMember> {
    let flag = |name| member.attribute(name) == Some("1");
    Some(ZoneMember {
        uuid: non_empty(member.attribute("UUID"))?,
        name: member
            .attribute("ZoneName")
            .unwrap_or_default()
            .trim()
            .to_owned(),
        location: non_empty(member.attribute("Location")),
        visible: !flag("Invisible") && !flag("IsZoneBridge"),
    })
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// AVTransport's `CurrentTransportState`. `NO_MEDIA_PRESENT` counts as stopped; a state UPnP
/// doesn't define is unknown.
pub(crate) fn transport_state(state: &str) -> Option<TransportState> {
    match state.trim() {
        "PLAYING" => Some(TransportState::Playing),
        "PAUSED_PLAYBACK" | "PAUSED_RECORDING" => Some(TransportState::Paused),
        "STOPPED" | "NO_MEDIA_PRESENT" => Some(TransportState::Stopped),
        "TRANSITIONING" => Some(TransportState::Transitioning),
        _ => None,
    }
}

/// A track's title and creator from its DIDL-Lite metadata (`TrackMetaData`), each if present
/// and not blank. Metadata that isn't DIDL (empty, `NOT_IMPLEMENTED`, broken) has neither.
pub(crate) fn title_and_creator(didl: &str) -> (Option<String>, Option<String>) {
    let Ok(document) = parse(didl) else {
        return (None, None);
    };
    let text = |name: &str| {
        document
            .descendants()
            .find(|node| node.has_tag_name((DC, name)))
            .and_then(|node| non_empty(node.text()))
    };
    (text("title"), text("creator"))
}

/// A UPnP time (`H+:MM:SS`, maybe with a fraction of a second, as `RelTime` and
/// `TrackDuration` carry it). `NOT_IMPLEMENTED` and anything else unreadable is none.
pub(crate) fn duration(time: &str) -> Option<Duration> {
    let mut parts = time.trim().split(':');
    let (hours, minutes, seconds) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, "0"));
    if !digits(hours) || !digits(minutes) || !digits(whole) || !digits(fraction) {
        return None;
    }
    let (minutes, whole): (u64, u64) = (minutes.parse().ok()?, whole.parse().ok()?);
    if minutes >= 60 || whole >= 60 {
        return None;
    }
    let hours: u64 = hours.parse().ok()?;
    let seconds = hours.checked_mul(3600)?.checked_add(minutes * 60 + whole)?;
    let fraction: f64 = format!("0.{fraction}").parse().ok()?;
    Some(Duration::from_secs(seconds) + Duration::from_secs_f64(fraction))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(uuid: &str, name: &str, visible: bool) -> ZoneMember {
        ZoneMember {
            uuid: uuid.into(),
            name: name.into(),
            location: Some(format!("http://192.168.1.2:1400/{uuid}.xml")),
            visible,
        }
    }

    #[test]
    fn reads_groups_with_visible_and_hidden_members() {
        // Shaped like current firmware: a Beam with bonded surrounds (as `Satellite`s and as
        // an invisible member), a grouped pair of rooms, and a Boost on its own.
        let state = r#"<ZoneGroupState><ZoneGroups>
            <ZoneGroup Coordinator="RINCON_BEAM" ID="RINCON_BEAM:1">
              <ZoneGroupMember UUID="RINCON_BEAM" ZoneName="Daniels Zimmer" Location="http://192.168.1.2:1400/RINCON_BEAM.xml">
                <Satellite UUID="RINCON_SL" ZoneName="Daniels Zimmer" Location="http://192.168.1.3:1400/x.xml" Invisible="1"/>
              </ZoneGroupMember>
              <ZoneGroupMember UUID="RINCON_SR" ZoneName="Daniels Zimmer" Location="http://192.168.1.2:1400/RINCON_SR.xml" Invisible="1"/>
            </ZoneGroup>
            <ZoneGroup Coordinator="RINCON_K" ID="RINCON_K:2">
              <ZoneGroupMember UUID="RINCON_B" ZoneName="Bath" Location="http://192.168.1.2:1400/RINCON_B.xml"/>
              <ZoneGroupMember UUID="RINCON_K" ZoneName="Kitchen" Location="http://192.168.1.2:1400/RINCON_K.xml"/>
            </ZoneGroup>
            <ZoneGroup Coordinator="RINCON_BOOST" ID="RINCON_BOOST:3">
              <ZoneGroupMember UUID="RINCON_BOOST" ZoneName="BOOST" Location="http://192.168.1.2:1400/RINCON_BOOST.xml" IsZoneBridge="1"/>
            </ZoneGroup>
          </ZoneGroups><VanishedDevices/></ZoneGroupState>"#;

        assert_eq!(
            zone_groups(state).unwrap(),
            [
                ZoneGroup {
                    coordinator: "RINCON_BEAM".into(),
                    members: vec![
                        member("RINCON_BEAM", "Daniels Zimmer", true),
                        member("RINCON_SR", "Daniels Zimmer", false),
                    ],
                },
                ZoneGroup {
                    coordinator: "RINCON_K".into(),
                    members: vec![
                        member("RINCON_B", "Bath", true),
                        member("RINCON_K", "Kitchen", true),
                    ],
                },
                ZoneGroup {
                    coordinator: "RINCON_BOOST".into(),
                    members: vec![member("RINCON_BOOST", "BOOST", false)],
                },
            ]
        );
    }

    #[test]
    fn reads_the_older_zone_groups_root() {
        let state = r#"<ZoneGroups><ZoneGroup Coordinator="RINCON_A" ID="x"><ZoneGroupMember UUID="RINCON_A" ZoneName="Office"/></ZoneGroup></ZoneGroups>"#;
        assert_eq!(
            zone_groups(state).unwrap(),
            [ZoneGroup {
                coordinator: "RINCON_A".into(),
                members: vec![ZoneMember {
                    uuid: "RINCON_A".into(),
                    name: "Office".into(),
                    location: None,
                    visible: true,
                }],
            }]
        );
    }

    #[test]
    fn leaves_out_groups_without_a_coordinator_and_members_without_a_uuid() {
        let state = r#"<ZoneGroups>
            <ZoneGroup ID="x"><ZoneGroupMember UUID="RINCON_A" ZoneName="A"/></ZoneGroup>
            <ZoneGroup Coordinator="RINCON_B"><ZoneGroupMember ZoneName="nameless"/><ZoneGroupMember UUID=" " ZoneName="blank"/></ZoneGroup>
          </ZoneGroups>"#;
        assert_eq!(
            zone_groups(state).unwrap(),
            [ZoneGroup {
                coordinator: "RINCON_B".into(),
                members: Vec::new(),
            }]
        );
    }

    #[test]
    fn broken_or_hostile_topology_is_an_error() {
        for state in [
            "",
            "not xml",
            "<ZoneGroups><ZoneGroup>",
            "<Other/>",
            r#"<!DOCTYPE z [<!ENTITY a "aaaa">]><ZoneGroups>&a;</ZoneGroups>"#,
        ] {
            assert!(zone_groups(state).is_err(), "{state:?}");
        }
        let huge = format!("<ZoneGroups>{}</ZoneGroups>", "<x/>".repeat(200_000));
        assert!(zone_groups(&huge).is_err());
    }

    #[test]
    fn reads_a_soap_actions_output_arguments() {
        let body = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:GetTransportInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><CurrentTransportState>PLAYING</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed></CurrentSpeed></u:GetTransportInfoResponse></s:Body></s:Envelope>"#;
        assert_eq!(
            soap_response(body, "GetTransportInfo").unwrap(),
            [
                ("CurrentTransportState".into(), "PLAYING".into()),
                ("CurrentTransportStatus".into(), "OK".into()),
                ("CurrentSpeed".into(), String::new()),
            ]
        );
        let fault = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring></s:Fault></s:Body></s:Envelope>"#;
        assert_eq!(
            soap_response(fault, "GetTransportInfo"),
            Err(Malformed::Missing("action response"))
        );
        assert!(soap_response("garbage", "GetTransportInfo").is_err());
    }

    #[test]
    fn transport_states_map_to_four_states() {
        for (state, expected) in [
            ("PLAYING", Some(TransportState::Playing)),
            ("PAUSED_PLAYBACK", Some(TransportState::Paused)),
            ("STOPPED", Some(TransportState::Stopped)),
            ("NO_MEDIA_PRESENT", Some(TransportState::Stopped)),
            ("TRANSITIONING", Some(TransportState::Transitioning)),
            ("RECORDING", None),
            ("", None),
        ] {
            assert_eq!(transport_state(state), expected, "{state:?}");
        }
    }

    #[test]
    fn reads_title_and_creator_from_didl() {
        let didl = r#"<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" xmlns:r="urn:schemas-rinconnetworks-com:metadata-1-0/" xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"><item id="-1" parentID="-1" restricted="true"><res protocolInfo="http-get:*:audio/mpeg:*" duration="0:04:38">http://bridge/a.mp3</res><r:streamContent></r:streamContent><dc:title>001 - Etymology</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class><dc:creator>Herman Melville</dc:creator><upnp:album>Moby Dick</upnp:album></item></DIDL-Lite>"#;
        assert_eq!(
            title_and_creator(didl),
            (
                Some("001 - Etymology".into()),
                Some("Herman Melville".into())
            )
        );
    }

    #[test]
    fn didl_without_title_or_that_is_not_didl_has_neither() {
        let blank = r#"<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/"><item><dc:title> </dc:title></item></DIDL-Lite>"#;
        // A title outside the Dublin Core namespace is not the track's title.
        let foreign = r#"<DIDL-Lite xmlns:x="urn:x"><item><x:title>t</x:title></item></DIDL-Lite>"#;
        for didl in ["", "NOT_IMPLEMENTED", "<DIDL-Lite", blank, foreign] {
            assert_eq!(title_and_creator(didl), (None, None), "{didl:?}");
        }
    }

    #[test]
    fn reads_upnp_times() {
        for (time, expected) in [
            ("0:00:00", Some(Duration::ZERO)),
            ("0:01:05", Some(Duration::from_secs(65))),
            ("7:04:14", Some(Duration::from_secs(25_454))),
            ("123:00:01", Some(Duration::from_secs(442_801))),
            ("0:00:01.500", Some(Duration::from_millis(1500))),
            ("NOT_IMPLEMENTED", None),
            ("", None),
            ("1:2", None),
            ("0:60:00", None),
            ("0:00:60", None),
            ("-1:00:00", None),
            ("0:00:0x", None),
            ("1:00:00:00", None),
            ("99999999999999999999:00:00", None),
        ] {
            assert_eq!(duration(time), expected, "{time:?}");
        }
    }
}
