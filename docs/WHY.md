# Shelfcast — Project Kick-off Brief

> Podcast playback that hands off from your phone to Sonos, powered by Audiobookshelf.

This brief summarizes the initial research and decisions. It is the input for a grilling session: challenge every assumption, surface gaps, and turn open questions into decisions before any code is written.

## 1. Goal

Spotify Connect–style podcast playback on Sonos:

- Start an episode from a phone app, hand it off to a Sonos group.
- The **speaker takes over** — it streams directly from the server. The phone app can be closed.
- **Not AirPlay.** No audio is streamed from the phone.
- Listening progress syncs back so any client (phone, web, Sonos) resumes at the same position.

Primary scope: personal / household use. Public release (Sonos partner program, App Store) is a possible later step, not a v1 requirement.

## 2. Key decisions so far

| Topic | Decision | Rationale |
|---|---|---|
| Podcast backend | **Audiobookshelf (ABS)**, already self-hosted | Handles feeds, downloads, per-user progress, has a documented API. |
| Overcast | **Not used** | No public API. Only unofficial scraping (login + OPML export) — read-only, fragile, no write-back of progress. |
| Sonos mechanism | **Cloud Queue + Control API** (not AirPlay) | Players keep playing and fetching tracks from the cloud queue after the app leaves the network. |
| Progress sync | Sonos `POST /timePlayed` reports → ABS `PATCH /api/me/progress/:libraryItemId/:episodeId` | Players report `positionMillis`; periodic reports configurable via `reports.periodicIntervalMillis`. |
| Platforms | iOS first, Android later | |
| Name | **Shelfcast** | Fits the ABS ecosystem naming (ShelfPlayer, ShelfDroid). No existing product found by web search. "Roomcast" rejected (TeleAdapt, Yealink). |
| Repo naming | `shelfcast` (umbrella); `shelfcast-ios`, `shelfcast-android`, `shelfcast-bridge` | |

## 3. Architecture

```
┌──────────────┐  Control API (LAN, secure WebSocket)   ┌──────────────┐
│ shelfcast-ios│ ─────────────────────────────────────▶ │ Sonos group  │
│  (phone app) │   createSession / loadCloudQueue       │  (players)   │
└──────┬───────┘                                        └──────┬───────┘
       │ ABS API (browse, progress)                            │ Cloud Queue API
       ▼                                                       │ SMAPI (SOAP)
┌──────────────┐        ABS API (items, progress)       ┌──────▼───────┐
│ Audiobookshelf│ ◀──────────────────────────────────── │shelfcast-    │
│   (existing)  │                                       │  bridge      │
└──────┬───────┘                                        └──────────────┘
       │ media stream (LAN URL + token)
       └────────────────────────────────▶ Sonos players fetch audio directly
```

### 3.1 `shelfcast-bridge` (server, Docker next to ABS)

- **Cloud Queue API**: `GET /context`, `GET /itemWindow`, `GET /version`, `POST /timePlayed`.
- **Minimal SMAPI service** (SOAP/WSDL) — required: the content service must be integrated with Sonos before the Control API can drive players. For dev/personal use it is registered as a custom service; a non-SSL LAN endpoint is accepted for testing.
- Reads episodes from ABS. Media URL = ABS stream URL on the LAN; ABS token passed to players via `httpAuthorization` (and `useHttpAuthorizationForMedia`).
- Writes progress to ABS from `/timePlayed` reports (periodic `update` + `final`).
- Suggested stack: Node/TypeScript (open for discussion).

### 3.2 `shelfcast-ios`

- Browses ABS libraries/podcasts/episodes via ABS API.
- "Play on Sonos": discover players/groups, open playback session, `loadCloudQueue` with `playOnCompletion: true` and `trackMetadata` for fast start; start position from ABS progress.
- Subscribe to `playbackSession` events; handle `ERROR_SESSION_EVICTED` (someone else took over the group).
- Candidate base: **ShelfPlayer** (open source, Swift 6, iOS 18+, ABS client with progress sync) — fork and add Sonos, vs. build from scratch.

### 3.3 Podcast search (improvement over ABS)

ABS search results were weak in testing (ABS likely uses the iTunes Search API — unverified). Candidate sources to combine, dedupe (by feed URL / iTunes ID), and rank:

- iTunes Search API with `country=at`, `de` (+ `us`), cached (~20 calls/min limit).
- Podcast Index (free key; `search/byterm`, `podcasts/byitunesid`).
- fyyd (German directory).
- Listen Notes / Taddy (paid).
- Popularity signal for ranking: Apple Podcast charts per country.

Overcast's search quality comes mainly from its own popularity-based ranking, not a special source.

## 4. Open questions for the grilling session

**Scope & product**
1. v1 = podcasts only, or audiobooks too (ABS supports both; chapters, long files)?
2. Is the phone app also a local player (play on phone + hand off), or a pure Sonos remote?
3. Queue model: single episode, "Up Next" from ABS, or a Shelfcast-managed queue?
4. Is improved search in v1 scope, or later?

**Sonos integration**
5. Exact steps to register a custom SMAPI service on current S2 firmware — verify against current docs.
6. Which Control API path: local LAN WebSocket only, or also the Sonos cloud Control API (needs developer account/OAuth)? Implications for "control from outside the house".
7. Does `loadCloudQueue` accept a start position, or must the bridge serve it via `/itemWindow`/`/context`? Verify.
8. Which playback policies (seek, skip, rate) to set; is playback speed possible at all on Sonos?
9. Behavior when the group is changed or another source takes over — how does the app reflect it?

**ABS integration**
10. Auth: per-user ABS API key vs. user token; how the bridge maps a Sonos session to an ABS user (multi-user household).
11. Progress conflict resolution when phone and Sonos both report (use `lastUpdate`, batch endpoint?).
12. Stream URL format and token lifetime for Sonos fetching ABS media; transcoding needs?

**Infrastructure & ops**
13. Hosting: same Docker host as ABS; HTTPS needed for which parts (dev vs. release)?
14. Monorepo (`shelfcast`) vs. separate repos — the naming above allows both.
15. Logging/debugging strategy for SMAPI (players give little feedback on errors).

**Release path (later)**
16. Personal use only, or aim for Sonos partner submission + App Store? Affects naming, branding, and auth design now.

## 5. Constraints & conventions

- All code, identifiers, comments, and config in **English**.
- Do not trust remembered API details — verify against current docs (Sonos, ABS) before implementing.
- Avoid "Sonos"/"Connect" in product names (trademark, App Store review); nominative use in descriptions is fine.

## 6. References

- Sonos — Play audio (Cloud queue): https://docs.sonos.com/docs/play-audio-cloud-queue
- Sonos — loadCloudQueue: https://docs.sonos.com/reference/playbacksession-loadcloudqueue-sessionid
- Sonos — Playback sessions: https://docs.sonos.com/docs/playback-sessions
- Sonos — Components and interfaces: https://docs.sonos.com/docs/components-and-interfaces
- Sonos — reports object: https://docs.sonos.com/reference/reports-object
- Sonos — timePlayed item attributes (v2.0): https://docs.sonos.com/reference/v20-item-attributes
- Sonos — Add reporting: https://docs.sonos.com/docs/add-reporting
- ABS API — Playback & progress: https://deepwiki.com/audiobookshelf/audiobookshelf-api-docs/3.6-playback-and-progress-tracking
- ShelfPlayer: https://apps.apple.com/app/id6475221163
- iTunes Search API: https://performance-partners.apple.com/resources/documentation/itunes-store-web-service-search-api/
- Podcast Index API: https://github.com/jamescridland/docs-api
- Listen Notes API: https://listennotes.com/de/api/docs/
- Taddy API: https://taddy.org/developers
