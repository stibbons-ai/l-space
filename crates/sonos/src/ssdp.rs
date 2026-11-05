//! Finding players with SSDP: one M-SEARCH for Sonos ZonePlayers, sent to the standard
//! multicast address (or, in tests, wherever `sonos-fake` listens), and the unicast answers
//! that arrive within the search window.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use tokio::net::UdpSocket;
use url::Url;

use crate::RAW;

/// SSDP's multicast group and port, where every UPnP device on the LAN listens.
pub const MULTICAST: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900));

/// The device type Sonos players announce.
const ZONE_PLAYER: &str = "urn:schemas-upnp-org:device:ZonePlayer:1";

/// The longest answer read; a Sonos answer is a few hundred bytes.
const MAX_DATAGRAM: usize = 2048;

/// A player that answered a search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub uuid: String,
    /// `http://<address>:<port>`, where its UPnP services answer.
    pub base_url: String,
}

/// Sends one M-SEARCH to `target` and collects the players that answer within `window`,
/// once each.
pub(crate) async fn search(target: SocketAddr, window: Duration) -> io::Result<Vec<Found>> {
    let any = match target {
        SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    };
    let socket = UdpSocket::bind((any, 0)).await?;
    // MX: players answer within a second, so the window can stay short.
    let request = format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {target}\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: {ZONE_PLAYER}\r\n\r\n"
    );
    tracing::debug!(target: RAW, peer = %target, datagram = request, "SSDP datagram sent");
    socket.send_to(request.as_bytes(), target).await?;

    let deadline = tokio::time::Instant::now() + window;
    let mut found: Vec<Found> = Vec::new();
    let mut buffer = [0; MAX_DATAGRAM];
    while let Ok(received) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await
    {
        let (length, from) = received?;
        let datagram = String::from_utf8_lossy(&buffer[..length]);
        tracing::debug!(target: RAW, peer = %from, datagram = %datagram, "SSDP datagram received");
        match answer(&datagram, from) {
            Some(player) if !found.iter().any(|known| known.uuid == player.uuid) => {
                found.push(player);
            }
            Some(_) => {}
            None => {
                tracing::debug!(peer = %from, "ignored an SSDP answer that is not a Sonos player's")
            }
        }
    }
    Ok(found)
}

/// The player in an M-SEARCH answer from `from`: a `200` for [`ZONE_PLAYER`] with a
/// `uuid:` USN and an `http` LOCATION at the address the answer came from. A LOCATION
/// elsewhere is refused, so an answer can't point the Bridge at another host.
fn answer(datagram: &str, from: SocketAddr) -> Option<Found> {
    let mut lines = datagram.split("\r\n");
    let status = lines.next()?;
    if !status.starts_with("HTTP/1.1 200") {
        return None;
    }
    let (mut search_target, mut usn, mut location) = (None, None, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_uppercase().as_str() {
            "ST" => search_target = Some(value),
            "USN" => usn = Some(value),
            "LOCATION" => location = Some(value),
            _ => {}
        }
    }
    if search_target? != ZONE_PLAYER {
        return None;
    }
    let uuid = usn?.strip_prefix("uuid:")?.split("::").next()?.trim();
    if uuid.is_empty() {
        return None;
    }
    let (host, base_url) = base_url(location?)?;
    if host != from.ip() {
        return None;
    }
    Some(Found {
        uuid: uuid.to_owned(),
        base_url,
    })
}

/// The IP address of a player's device description URL (`location`) and the base URL of its
/// services there, `http://<ip>:<port>`; none unless it is `http` to an IP address.
pub(crate) fn base_url(location: &str) -> Option<(IpAddr, String)> {
    let location = Url::parse(location).ok()?;
    let host: IpAddr = match location.host()? {
        url::Host::Ipv4(ip) => ip.into(),
        url::Host::Ipv6(ip) => ip.into(),
        url::Host::Domain(_) => return None,
    };
    if location.scheme() != "http" {
        return None;
    }
    let port = location.port_or_known_default()?;
    Some((host, format!("http://{}", SocketAddr::new(host, port))))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FROM: &str = "192.168.1.23:1900";

    fn datagram(st: &str, usn: &str, location: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age = 1800\r\nEXT:\r\nLOCATION: {location}\r\nSERVER: Linux UPnP/1.0 Sonos/97.2-81220 (ZPS14)\r\nST: {st}\r\nUSN: {usn}\r\nX-RINCON-HOUSEHOLD: Sonos_x\r\n\r\n"
        )
    }

    fn read(datagram: &str) -> Option<Found> {
        answer(datagram, FROM.parse().unwrap())
    }

    #[test]
    fn reads_a_sonos_answer() {
        let found = read(&datagram(
            ZONE_PLAYER,
            "uuid:RINCON_48A6B8000001400::urn:schemas-upnp-org:device:ZonePlayer:1",
            "http://192.168.1.23:1400/xml/device_description.xml",
        ));
        assert_eq!(
            found,
            Some(Found {
                uuid: "RINCON_48A6B8000001400".into(),
                base_url: "http://192.168.1.23:1400".into(),
            })
        );
        // Header names in any case.
        let lower = "HTTP/1.1 200 OK\r\nlocation: http://192.168.1.23/d.xml\r\nst: urn:schemas-upnp-org:device:ZonePlayer:1\r\nusn: uuid:RINCON_A\r\n\r\n";
        assert_eq!(
            read(lower),
            Some(Found {
                uuid: "RINCON_A".into(),
                base_url: "http://192.168.1.23:80".into(),
            })
        );
    }

    #[test]
    fn refuses_other_devices_and_broken_or_misdirecting_answers() {
        let usn = "uuid:RINCON_A::urn:schemas-upnp-org:device:ZonePlayer:1";
        let here = "http://192.168.1.23:1400/xml/device_description.xml";
        for datagram in [
            String::new(),
            "garbage".into(),
            datagram(ZONE_PLAYER, usn, here).replace("200 OK", "404 Not Found"),
            datagram("urn:schemas-upnp-org:device:MediaRenderer:1", usn, here),
            datagram(ZONE_PLAYER, "RINCON_A", here),
            datagram(ZONE_PLAYER, "uuid:::x", here),
            datagram(ZONE_PLAYER, usn, "not a url"),
            datagram(ZONE_PLAYER, usn, "https://192.168.1.23:1443/x.xml"),
            datagram(ZONE_PLAYER, usn, "http://192.168.1.99:1400/x.xml"),
            datagram(ZONE_PLAYER, usn, "http://sonos.lan:1400/x.xml"),
            "HTTP/1.1 200 OK\r\nST: urn:schemas-upnp-org:device:ZonePlayer:1\r\n\r\n".into(),
        ] {
            assert_eq!(read(&datagram), None, "{datagram:?}");
        }
    }
}
