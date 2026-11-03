# The Bridge controls Sonos over local UPnP

The Bridge drives Groups through Sonos's local UPnP interface: it finds players with SSDP, uses AVTransport actions to play, seek and pause, and watches playback through GENA events. Groups play media URLs served by the Bridge (ADR 0010). We chose this over the official cloud Control API because it needs no Sonos developer account, OAuth, trusted TLS hostname or internet connection. That matters because every household that builds L-Space has to repeat that setup. Sonos calls UPnP "unsupported" and its 2025 security advisory recommends turning it off. L-Space therefore requires the UPnP setting in the Sonos app (Account → Privacy & Security → Connection Security, on by default) to stay enabled.

## Considered Options

- **Official cloud Control API + Cloud Queue**: officially supported, but every household needs its own Sonos developer integration, OAuth, and a hostname with a publicly trusted certificate. Whether `createSession` works without a registered SMAPI service is unverified. This remains the fallback, so Sonos control sits behind a single interface inside the Bridge.
- **Unofficial LAN Control API (WebSocket on port 1443)**: uses a hardcoded key Sonos can revoke, is "not available for wide release", and nobody has shown it working with a LAN cloud-queue server.
- **Custom SMAPI service (customsd)**: since a May 2024 S2 update it must be reachable over public HTTPS.
