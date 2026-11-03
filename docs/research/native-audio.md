# L-Space: native audio on iOS (Dioxus vs Tauri 2 vs SwiftUI)
Researched 2026-10-24.

## 1. HTML `<audio>` in WKWebView (native host app)
- **Background/locked playback: yes**, if the host sets `AVAudioSession` `.playback` and `UIBackgroundModes=[audio]`. A field spike on a real iPhone ran 13 minutes locked and auto-advanced 21 tracks: https://github.com/emoonadev/Kaset-iOS/blob/main/docs/spike-findings.md. Apple's docs on the category and background mode: https://developer.apple.com/documentation/AVFoundation/configuring-your-app-for-media-playback
- **Main fragility:** if you pause and then wait, iOS freezes the WebContent process (152 s seen), clears the Now Playing widget and stalls the stream. The spike only fixed this with a silent, always-running `AudioContext` hack. Pausing in the foreground and then locking straight away still suspended the whole app (same URL). Someone else reports background playback dying after about 3 s: https://stackoverflow.com/questions/79585537
- **Media Session API:** shipped in Safari/WebKit 15, so iOS 15+: https://webkit.org/blog/11989/new-webkit-features-in-safari-15/ and https://developer.mozilla.org/en-US/docs/Web/API/MediaSession. In WKWebView, page-level `setActionHandler('nexttrack'…)` handlers did reach the page from the lock screen. Without them the lock screen shows ±10 s skip buttons (Kaset spike).
- Known WebKit bugs: lock-screen artwork missing (https://bugs.webkit.org/show_bug.cgi?id=251782); autoplay and Media Session controls broken in standalone PWAs on iOS 16/17 (https://bugs.webkit.org/show_bug.cgi?id=261858).
- **Interruptions (phone calls):** WebKit handles these internally. There is no JS event for an interruption ending with "should resume". **UNVERIFIED** (the spike lists phone calls and Bluetooth as untested).
- **AirPlay:** `WKWebViewConfiguration.allowsAirPlayForMediaPlayback` defaults to true: https://developer.apple.com/documentation/webkit/wkwebviewconfiguration/allowsairplayformediaplayback. The `x-webkit-airplay="allow"` attribute and the `webkitShowPlaybackTargetPicker()` picker inside WKWebView: **UNVERIFIED**. A native AVRoutePickerView would also work.
- **HTTP range requests:** AVFoundation inside WebKit does range requests natively. If you use a custom-scheme or asset-protocol handler, it has to answer `Range` with 206 responses (**UNVERIFIED** for the dx asset handler).
- **Bugs specific to iOS 18 or 26:** none found in primary sources (**UNVERIFIED**).

## 2. Native AVPlayer from Rust (objc2)
- `objc2-av-foundation` 0.3.2 has `AVPlayer` and `AVPlayerItem` with iOS targets documented: https://docs.rs/objc2-av-foundation/0.3.2/aarch64-apple-ios/objc2_av_foundation/struct.AVPlayer.html (AVAudioSession is in the same crate).
- `objc2-media-player` 0.3.2 (released 3 days ago) has `MPNowPlayingInfoCenter`, `MPNowPlayingSession` and `MPRemoteCommandCenter`: https://docs.rs/crate/objc2-media-player/latest/features and https://docs.rs/objc2-media-player. Remote-command handlers take blocks, which means `block2`, plus `MainThreadMarker`. That is workable but verbose.
- `objc2-av-kit` has `AVRoutePickerView`: https://docs.rs/objc2-av-kit/latest/objc2_av_kit. It is a UIView, so it has to sit on top of the WKWebView as a native subview.
- Dioxus itself was ported to objc2 (https://github.com/dioxuslabs/dioxus/releases), so the crates fit its dependency tree. No ready-made Dioxus audio or Now Playing crate exists (**UNVERIFIED**: search found none). Expect to write the bridge yourself.
- For comparison, Apple's new Swift-only Now Playing framework (WWDC26) is not reachable from objc2: https://developer.apple.com/videos/play/wwdc2026/312

## 3. Customising the dx 0.7 iOS build
- Since 0.7 (expanded in 0.7.4), `Dioxus.toml` lets you set `[ios.plist]` and `[ios.entitlements]` keys:
  - https://dioxuslabs.com/blog/release-070
  - https://github.com/DioxusLabs/dioxus/blob/main/notes/architecture/12-MANIFEST-SYSTEM.md
  - https://dioxuslabs.com/learn/0.7/guides/tools/configure
  - https://github.com/dioxuslabs/dioxus/releases

  So `UIBackgroundModes=[audio]` is a supported setting.
- **Swift interop:** `#[manganis::ffi("/src/ios")] extern "Swift" { … }` builds a SwiftPM package and generates bindings (primitives, String, Option, opaque types): https://docs.rs/dioxus/latest/dioxus/prelude/manganis/attr.ffi.html and https://docs.rs/manganis/latest/manganis
- The FFI is young. A recent fix closed a leak in the iOS FFI wrappers (Retained): https://github.com/dioxuslabs/dioxus/releases. A third-party fork exists to carry the iOS FFI patch: https://git.swaits.com/swaits/dioxus/src/commit/702fbac2dabcc358846fe1cdc293fb4165c27eba
- Not confirmed (**UNVERIFIED**):
  - callbacks from Swift into Rust (remote commands);
  - adding a native subview via `dioxus-mobile`. It is a re-export of dioxus-desktop/wry (https://docs.rs/crate/dioxus-mobile/0.7.0-alpha.2), and the window or webview is presumably reachable via objc2.

## 4. Tauri 2
- There is a documented Swift plugin system (a `Plugin` subclass with `@objc` commands, an event trigger back to JS, and an editable Xcode project and Info.plist): https://v2.tauri.app/develop/plugins/develop-mobile/
- Existing plugins:
  - `tauri-plugin-native-audio`: iOS AVPlayer + MPNowPlayingInfoCenter + MPRemoteCommandCenter. https://github.com/uvarov-frontend/tauri-plugin-native-audio and https://docs.rs/crate/tauri-plugin-native-audio/1.0.1/source/README.md
  - `tauri-plugin-media-session`: Now Playing, docs say add UIBackgroundModes audio. https://docs.rs/crate/tauri-plugin-media-session/latest/source/README.md
  - `tauri-plugin-media`: https://github.com/Taiizor/tauri-plugin-media
  - `tauri-plugin-audio`: https://github.com/stippi/tauri-plugin-audio
- Quality and maintenance of these plugins: **UNVERIFIED**.

## 5. Risk assessment
| Option | Feasibility / effort |
|---|---|
| Dioxus + HTML `<audio>` | Cheapest. Locked playback and Media Session work, but WebContent suspension on pause needs a silent-AudioContext hack. Interruption and AirPlay picker behaviour is opaque. Medium–high reliability risk. |
| Dioxus + objc2 AVPlayer (or manganis Swift FFI) | Feasible: all APIs are bound and plist/entitlements are supported. Effort is 1–2 weeks of FFI glue for callbacks, the main thread, KVO and the route-picker overlay. Medium risk from young FFI tooling and no prior examples. |
| Tauri 2 + Swift plugin | Most mature hybrid path: an official Swift plugin API and existing AVPlayer/Now Playing plugins to fork. Low–medium effort, but it means migrating the UI from Dioxus to a JS/TS (or Leptos/Yew) frontend. |
| SwiftUI app | Lowest platform risk and full access (AVPlayer, the new Now Playing framework, AVRoutePickerView). Highest rewrite cost and no shared Rust UI; Rust core is possible via UniFFI (**UNVERIFIED** fit). |

**Recommendation:** stay on Dioxus but play audio natively. Write a small Swift package for AVPlayer, Now Playing and remote commands, called through `manganis::ffi` (or objc2 directly), and set background audio in `[ios.plist]`. Prototype remote-command callbacks into Rust first. If that fails, fall back to Tauri 2 with `tauri-plugin-native-audio`.
