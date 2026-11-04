# The App is a Dioxus mobile app, sideloaded rather than published

The App is built with Dioxus: a Rust UI rendered in a native WKWebView shell and signed with our own Apple developer account. We chose it over SwiftUI because it shares Rust types with the Bridge through server functions without code generation, and the same codebase can later build Android and web. The UI does not look fully native, and native extras such as Live Activities or widgets will need separate Swift extensions.

Local playback does not use HTML audio. After a pause, iOS freezes the WebKit content process and clears Now Playing. Instead, a `LocalPlayer` trait has an iOS implementation that calls AVPlayer, AVAudioSession, MPNowPlayingInfoCenter and MPRemoteCommandCenter from Rust through the `objc2` crates. Background audio is enabled through `[ios.plist]` in `Dioxus.toml`.

## Gate

This decision holds only if a spike on a physical iPhone shows all of the following:

1. a `dx` build installs on the device;
2. a Bridge stream keeps playing for 10+ minutes while the phone is locked;
3. lock-screen pause and resume reach Rust;
4. a phone call interrupts playback and it resumes afterwards;
5. switching to an AirPlay output from Control Center keeps playing.

## Gate result (2026-11-04): Dioxus holds, on Dioxus 0.8

The spike in `spikes/ios-audio` (issue #2) ran a release `dx` build on an iPhone 17 Pro with iOS 27, built with Xcode 27. It played an HTTP MP3 stream through AVPlayer driven from Rust via `objc2`. The evidence is the spike's in-app log.

| # | Check | Result |
|---|-------|--------|
| 1 | `dx` build installs | Pass, but only on Dioxus 0.8 (see below) |
| 2 | 10+ minutes locked | Pass: 11 minutes of 60-second heartbeats while playing, with no stalls |
| 3 | Lock-screen pause and resume reach Rust | Pass: `MPRemoteCommandCenter` pause and play handlers ran in Rust |
| 4 | A call interrupts and playback resumes | Pass: interruption began, then ended with `shouldResume`, and playback resumed |
| 5 | AirPlay from Control Center | Pass: the route changed to AirPlay and playback continued |

AirPods also worked. Taking them out paused playback, as iOS does when headphones disconnect, and the next play used the speaker.

Findings that bind the App:

- **Dioxus 0.8, not 0.7.** With the iOS 27 SDK, apps that don't adopt the UIScene life cycle abort at launch. Dioxus 0.7.10 uses tao 0.34, which has no scene support. Dioxus 0.8 (0.8.0-alpha.3 at the time of the spike) uses tao 0.37, which does. The App needs Dioxus 0.8 and `dx` 0.8, so the workspace's `dioxus` pin moves with it.
- **A static scene manifest.** The iOS 27 launch check reads only `Info.plist`, so `Dioxus.toml` must declare `UIApplicationSceneManifest` with `TaoSceneDelegate` as the scene delegate.
- **The Swift runtime path.** In the iOS 27 SDK, CoreMedia links `@rpath/libswiftCoreMedia.dylib`, and `dx` doesn't add `/usr/lib/swift` to the runpath the way Xcode does. A `build.rs` passes `-Wl,-rpath,/usr/lib/swift`.
- **Signing.** `dx` signs with any Apple Development profile matching the bundle ID. Xcode's wildcard team profile works.

## Considered Options

- **SwiftUI**: the best iOS feel, but API types would come from code generation (OpenAPI or protobuf) and Android would be a separate app.
- **Tauri 2 wrapping a Rust web UI**: stable mobile tooling and a Swift plugin system with existing native-audio plugins, but it adds a second framework layer. This is the fallback if the spike fails: the Dioxus UI would run as a web build inside Tauri, with `tauri-plugin-native-audio` for Local playback.
- **PWA**: needs no signing, but the App was wanted as a native install.
