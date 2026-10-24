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

## Considered Options

- **SwiftUI**: the best iOS feel, but API types would come from code generation (OpenAPI or protobuf) and Android would be a separate app.
- **Tauri 2 wrapping a Rust web UI**: stable mobile tooling and a Swift plugin system with existing native-audio plugins, but it adds a second framework layer. This is the fallback if the spike fails: the Dioxus UI would run as a web build inside Tauri, with `tauri-plugin-native-audio` for Local playback.
- **PWA**: needs no signing, but the App was wanted as a native install.
