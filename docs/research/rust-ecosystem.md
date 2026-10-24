# Shelfcast — Rust ecosystem findings (2026-10-24)

## 1. Rust UPnP / Sonos crates
| crate | version | last release | notes |
|---|---|---|---|
| rupnp | 3.0.0 | 2025-04-25 (last commit 2025-06-30) | SSDP discovery, arbitrary `Service::action(url, name, args)`, SCPD parsing, GENA `subscribe`/`renew_subscription`/`unsubscribe` (feature `subscribe`) |
| ssdp-client | 2.1.0 | 2025-04-25 | same author; async M-SEARCH only |
| sonor | 2.0.0 | 2023-09-16 (last commit 2025-04-25) | Sonos wrapper over rupnp (play/seek/queue/track info); thin, effectively unmaintained |

Sources: crates.io API (https://crates.io/api/v1/crates/rupnp, …/sonor, …/ssdp-client); https://docs.rs/rupnp/latest/rupnp/; https://github.com/jakobhellermann/rupnp/blob/master/src/service.rs; https://github.com/jakobhellermann/sonor

Key detail: rupnp `subscribe()` binds its **own** TcpListener on an ephemeral port of the detected local addr and returns a Stream of `HashMap<String,String>` (src/service.rs above). So: no axum integration, needs `network_mode: host` in Docker (also needed for SSDP multicast anyway), and LastChange XML (DIDL/escaped) parsing is left to you.

Verdict: rupnp is usable (one maintainer, low cadence, small API, works). Norm for a serious bridge = rupnp or ssdp-client for discovery + reqwest SOAP (hand-built envelopes for ~8 AVTransport/RenderingControl actions) + quick-xml + own axum `NOTIFY` route on a fixed port (Docker-friendly, single callback server for all subscriptions, explicit renewal). sonor adds little. Other Rust Sonos crates: none found with recent activity — UNVERIFIED (did not exhaustively search crates.io).

Reference for Sonos service SOAP shapes: https://sonos.svrooij.io/services/av-transport (svrooij/sonos-api-docs, generated from device SCPD).

## 2. Sonos sleep timer over UPnP
- `AVTransport#ConfigureSleepTimer(InstanceID=0, NewSleepTimerDuration="HH:MM:SS" or "" to cancel)` and `GetRemainingSleepTimerDuration` → `RemainingSleepTimerDuration`, `CurrentSleepTimerGeneration`: https://sonos.svrooij.io/services/av-transport ; https://github.com/svrooij/sonos-api-docs/blob/main/docs/services/av-transport.md
- SoCo implements `set_sleep_timer(seconds|None)` / `get_sleep_timer()` via exactly those actions: https://github.com/SoCo/SoCo/blob/master/soco/core.py (≈L2820–2880). SoCo users run it nightly via Pi (https://en.community.sonos.com/controllers-and-music-services-229131/feature-request-remember-sleep-timer-state-6880772).
- S2 support: svrooij docs are generated from current (S2-era) firmware and SoCo is actively used with S2; no explicit S2 confirmation found — UNVERIFIED, test on device.
- Behavior on fire: svrooij doc text says "Stop playing after set sleep timer"; whether transport goes to PAUSED_PLAYBACK vs STOPPED is UNVERIFIED (my expectation: pause, since app-side resume keeps position — verify via GENA TransportState). Treat it as "playback halts"; read position via GetPositionInfo on the state change and push to ABS. Timer is per group coordinator; generation counter changes on set/cancel (doc above).

## 3. Leptos vs Dioxus
- Leptos 0.8.22 (2026-10-15), Dioxus 0.7.10 + dioxus-cli 0.7.10 (2026-10-16): crates.io API.
- Both: server functions (`#[server]`) share Rust types between client/server without codegen, axum-based server integration, SSR + hydration. Leptos: https://book.leptos.dev/server/25_server_functions.html, `leptos_axum`. Dioxus 0.7 fullstack (axum-native, server fns, SSR/hydration, hot-patching via Subsecond): https://dioxuslabs.com/blog/release-070
- PWA: neither has first-class PWA tooling; you add manifest + service worker yourself (Dioxus has a PWA example in repo, UNVERIFIED current state; Leptos none official) — UNVERIFIED.
- Dioxus iOS: `dx serve --platform ios` / `dx bundle` built into dx since 0.6 (https://dioxuslabs.com/blog/release-060), renders in WKWebView (wry), needs Xcode + iOS Rust targets (`rustup target add aarch64-apple-ios aarch64-apple-ios-sim`): https://dioxuslabs.com/learn/0.7/guides/platforms/mobile. Simulator is smooth; physical-device signing/provisioning for sideloading is the rough edge (codesigning flow documented less; community reports issues) — maturity: workable for personal use, UNVERIFIED for device deploy specifics.
- Leptos on iOS: no native renderer; wrap via Tauri 2 (CSR/static build in bundled assets, or point at remote URL). Common pattern.

## 4. Tauri 2 iOS
- Tauri 2 stable since 2.0 (Oct 2024) with iOS/Android: https://v2.tauri.app/blog/tauri-20/ ; current 2.12.5 (2026-10-21, crates.io API).
- Bundled frontend (any framework, incl. Leptos CSR) or remote URL (`build.frontendDist` / window URL; remote pages need capability `remote` config for IPC): https://v2.tauri.app/security/capabilities/ (remote IPC access — UNVERIFIED exact key name).
- Needs macOS + Xcode, `tauri ios init/dev/build`: https://v2.tauri.app/start/prerequisites/ ; native layer via Swift plugins: https://v2.tauri.app/develop/plugins/develop-mobile/
- Personal sideload: free Apple ID → 7-day provisioning, paid ($99/yr) → 1 year; Apple policy, UNVERIFIED citation (https://developer.apple.com/support/compare-memberships/). Effort: ~hours for wrapper.
- For "phone UI over LAN", a PWA from the bridge avoids all of this; native wrapper only buys lock-screen/background bits (which a WKWebView doesn't really give either).

## 5. Monorepo tooling
- moon supports Rust toolchain (enable `rust: {}` in `.moon/toolchains.yml`, parses Cargo manifests/lockfiles for hashing, syncs rust-toolchain.toml): https://moonrepo.dev/docs/guides/rust/handbook. No Swift/Xcode toolchain; use plain command tasks.
- For one Cargo workspace + small Xcode project: Cargo workspace + `just` (or mise tasks) is the low-ceremony norm; moon is overhead unless multi-language caching is wanted. (Opinion.)

## 6. Audiobookshelf API
Routes from source https://github.com/advplyr/audiobookshelf/blob/master/server/routers/ApiRouter.js (L174–243) and docs https://api.audiobookshelf.org/ / https://github.com/audiobookshelf/audiobookshelf-api-docs/blob/main/source/includes/_me.md:
- List episodes: `GET /api/items/:id` (podcast item, `media.episodes[]`); add `?expanded=1&include=progress` for progress (include param UNVERIFIED for episodes). Per-user progress: `GET /api/me/progress` (all) or `GET /api/me/progress/:libraryItemId/:episodeId` (`currentTime`, `duration`, `progress`, `isFinished`, `finishedAt`).
- Update/mark finished: `PATCH /api/me/progress/:libraryItemId/:episodeId` body `{currentTime, duration, progress, isFinished}`; batch `PATCH /api/me/progress/batch/update`; remove `DELETE /api/me/progress/:id`.
- Sessions: `POST /api/items/:id/play/:episodeId` opens a playback session, `POST /api/session/:id/sync` (`currentTime`, `timeListened`, `duration`), `POST /api/session/:id/close`; offline `POST /api/session/local`. Sessions drive listening stats/history and also update progress; plain PATCH progress only updates position/finished. For an external player: PATCH progress is sufficient for correctness; use sessions only if you want ABS listening stats ("time listened") and "open sessions" view. (session→stats semantics from docs; UNVERIFIED that sync alone updates progress in all versions.)
