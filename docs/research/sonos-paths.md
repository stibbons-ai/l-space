# L-Space — Sonos playback paths (research 2026-10-24)

## Q1. Local UPnP on current S2 firmware
- **Still works, and Sonos has made it an official user toggle.** Since a July 2025 update the Sonos app has Account > Privacy & Security > Connection Security with three toggles: Authentication (default OFF), **UPnP (default ON)**, and Guest access. Sonos calls UPnP an "unsupported protocol". Turning it off also breaks the Sonos macOS/Windows app. https://support.sonos.com/en-us/article/adjust-connection-security-settings ; announcement thread: https://en.community.sonos.com/product-updates/a-new-player-update-is-live-6930092/index3.html
- Sonos security advisory SSA-2025-0002 (2025-08-25) tells S2 users to **disable UPnP** as a mitigation. That is a real risk: a user who has followed the advice will break L-Space. https://www.sonos.com/en-in/security-advisory-2025-0002
- No formal deprecation date for UPnP was found. A "Legacy" split of older S2 players began 2025-12-16, but they still get maintenance updates. https://support.sonos.com/en-ca/article/release-notes-sonos-system-updates , https://support.sonos.com/en-gb/article/legacy-product-update
- **Home Assistant** (SoCo-based) docs list "UPnP must be enabled" as a prerequisite. It uses UPnP events and falls back to polling. https://www.home-assistant.io/integrations/sonos . **SoCo** `play_uri` uses AVTransport SetAVTransportURI: https://github.com/SoCo/SoCo/blob/master/soco/core.py
- AVTransport actions on S2 models are documented, including SetAVTransportURI, AddURIToQueue, Seek (REL_TIME) and GetPositionInfo: https://github.com/svrooij/sonos-api-docs/blob/main/docs/services/av-transport.md
- GENA subscriptions keep working on S2 because HA relies on them. Specific 2025-2026 firmware breakages of Seek or GENA: UNVERIFIED (none found).

## Q2. Media URLs via UPnP
- Plain `http:`/`https:` URLs to mp3/m4a files play as tracks. SoCo's docstring says `http:`, `https:`, `aac:` and `x-file-cifs:` are treated as track format. `x-rincon-mp3radio:` and `hls-radio:` force radio-style display and controls. Use plain http(s) for podcasts so seeking works. https://github.com/SoCo/SoCo/blob/master/soco/core.py (play_uri, force_radio). HA confirms "Direct HTTP/HTTPS links to local or remote media files" work if the player can reach them: https://www.home-assistant.io/integrations/sonos
- DIDL-Lite metadata (title, `upnp:albumArtURI`) is passed as CurrentURIMetaData. SoCo builds it from `meta` or `title`. Artwork display from DIDL with an arbitrary http URL: likely, but UNVERIFIED on current firmware.
- Query-string tokens: URLs are opaque to the player, so nothing should stop them. No source confirms this explicitly: UNVERIFIED (low risk).
- Players issue HTTP Range requests for mp3/mp4 when seeking or resuming, so the server must support `206`. https://docs.sonos.com/docs/playback-on-sonos
- HTTPS certificates: Sonos docs require a valid X.509 cert for SMAPI and cloud-queue endpoints. https://docs.sonos.com/docs/security , https://docs.sonos.com/docs/cloud-queue-play-audio . For UPnP stream URLs: UNVERIFIED. Community reports suggest self-signed certs fail, so **use plain HTTP on the LAN**.
- Redirects: UNVERIFIED. One community thread reports trouble after an HTTP→HTTPS redirect: https://en.community.sonos.com/controllers-and-music-services-229131/internet-radio-station-recently-switched-to-https-and-is-not-accessible-through-sonos-anymore-6855891 . Serve the final URL directly.
- Queue vs direct transport: SetAVTransportURI replaces the current source. AddURIToQueue plus playing from the queue (`x-rincon-queue:`) gives next/previous. HA supports both via `enqueue`. Differences in GetPositionInfo between the two: UNVERIFIED.

## Q3. Official cloud route with no partner program
- Any developer account can create a "Control integration" in the integration manager and get a client key/secret. OAuth authorization-code flow gives access to the user's own household. https://docs.sonos.com/docs/authorize , https://github.com/sonos/api-web-sample-app . A 2024 report says the portal showed no key-creation option: https://en.community.sonos.com/advanced-setups-229000/sonos-developer-portal-no-option-to-create-api-keys-6905690 (current state partly UNVERIFIED). Events require a public callback URL (the sample app uses ngrok).
- **Cloud Queue endpoints are fetched by the players themselves, not by the Sonos cloud.** Docs: "All Cloud Queue API requests from players to cloud queue servers will be over HTTPS" with a "well-known trusted CA" root. https://docs.sonos.com/docs/cloud-queue-play-audio . Inference: a LAN hostname works if it resolves on the LAN and has a publicly trusted cert (e.g. a Let's Encrypt DNS-01 cert on a split-DNS name). No public exposure would be needed. That last point is UNVERIFIED.
- SMAPI is not strictly required. Items may carry a `mediaUrl` instead of a `musicObjectId`; `musicObjectId` wins if both are present and resolves through SMAPI. https://docs.sonos.com/docs/cloud-queue-play-audio , https://docs.sonos.com/docs/playback-objects . However, `createSession` takes an `accountId` that must match a stored account (via `musicServiceAccounts.match` with a service ID). Whether a non-partner can create a session without a registered service ID: UNVERIFIED. `loadStreamUrl` exists for radio-style streams: https://docs.sonos.com/reference/playbacksession-loadstreamurl-sessionid
- SMAPI service registration for non-partners: only customsd (local test registration) exists, and it needs a public HTTPS endpoint per prior research. Production listing requires partner onboarding: https://docs.sonos.com/docs/smapi

## Q4. Unofficial LAN WebSocket (wss://player:1443/websocket/api)
- Sonos says the LAN Control API is "not available for wide release". It needs a client cert or approval, and the July 2025 "Authentication" toggle can gate it. https://en.community.sonos.com/advanced-setups-229133/where-is-the-lan-api-documented-6930288
- In use today: jjlawren/sonos-websocket (Home Assistant) uses an embedded `X-Sonos-Api-Key` for `audioClip`/`groups` only. https://raw.githubusercontent.com/jjlawren/sonos-websocket/main/sonos_websocket/websocket.py . sonn-audio/node-sonos claims "Cloud queue + stream loading via playbackSession" over 1443 with a self-signed cert. https://github.com/sonn-audio/node-sonos
- No public report was found of loadCloudQueue pointed at a **LAN plain-HTTP** queue server working: **UNVERIFIED**. Per Q3, players expect HTTPS with a trusted CA, so plain HTTP probably fails.

## Q5. Audiobookshelf auth
- API docs: "For GET requests the API token can optionally be passed in as a query string" (`?token=`). https://api.audiobookshelf.org/
- API keys exist since v2.26, as part of the new JWT auth system. Admin users create them. https://audiobookshelf.org/docs/documentation/server-management/api-keys
- Whether API keys (as opposed to legacy user tokens) are accepted via `?token=` on `/api/items/:id/file/:ino` or `/hls` streams after 2.26: UNVERIFIED. Test against the deployed version.

## Recommendation
UPnP (SoCo-style) is the only fully local path today. Serve plain HTTP on the LAN, send DIDL metadata, sync progress by polling GetPositionInfo plus GENA. Document the dependency on the UPnP toggle.
