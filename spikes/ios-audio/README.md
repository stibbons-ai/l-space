# iOS audio spike (issue #2, ADR 0007 gate)

Throwaway Dioxus app that plays an HTTP stream through native `AVPlayer` via `objc2`, not HTML audio.
Not a workspace member. Delete it once ADR 0007 records the outcome.

## What it does

- `AVAudioSession` category `playback`, `UIBackgroundModes = ["audio"]`.
- Lock-screen and Control Center play, pause and toggle go to Rust handlers (`MPRemoteCommandCenter`).
- Interruptions: logs begin and end; resumes on end when iOS sets `shouldResume`.
- Route changes: logs the new outputs, e.g. `Living Room [AirPlay]`.
- Heartbeat line every 60 s of playback, stall and failure notifications.
- The in-app log (newest first) is the evidence for every check; the same lines go to stderr with a `[spike]` prefix.

## Toolchain

`mise install` in this directory installs the Rust iOS targets and `dx` 0.8.0-alpha.3, which shadows Deno's `dx`.
`mise.toml` points `DEVELOPER_DIR` at Xcode.app, so `xcode-select` stays untouched.

Dioxus 0.7.10, the workspace pin, does not work here: its tao 0.34 has no UIScene support, and apps built
with the iOS 27 SDK abort at launch with "UIScene life cycle is required for apps built with this SDK".
Dioxus 0.8.0-alpha.3 ships tao 0.37, which does; `Dioxus.toml` declares the static scene manifest the
launch-time validator needs.

## One-time device setup (human)

1. Xcode → Settings → Accounts: sign in with the Apple ID of the developer team.
2. Plug in the iPhone, trust the Mac, enable Settings → Privacy & Security → Developer Mode, reboot.
3. Create a provisioning profile for `ai.stibbons.lspace.spike`: Xcode → new iOS App project with that
   bundle ID, your team, automatic signing, run it once on the phone. This creates the Apple Development
   certificate and the profile in `~/Library/Developer/Xcode/UserData/Provisioning Profiles`, where `dx`
   looks for it.
4. `security find-identity -v -p codesigning` shows the identity, e.g. `Apple Development: Name (TEAMID)`.

## Run on the device

```sh
mise exec -- xcrun devicectl list devices          # device name
mise exec -- dx serve --ios --release --device "<iPhone name>" \
  --codesign --apple-team-id "Apple Development: <Name> (<TEAMID>)"
```

`--release` because the gate is about real behaviour, not hot reload.

## Gate checklist

Tap **Load + play** (default: SomaFM Groove Salad, an endless MP3 stream). For a LAN source like the
Bridge, serve a file from the Mac with `python3 -m http.server 8000` and enter `http://<mac-ip>:8000/<file>`;
iOS asks for local network access once.

| # | Check | How | Evidence in the log |
|---|-------|-----|---------------------|
| 1 | `dx` build installs | the command above | app launches |
| 2 | 10+ min locked | lock the phone, wait 11 min, unlock | `heartbeat` lines every 60 s with no gap, no `stalled` |
| 3 | Lock-screen pause/resume reach Rust | lock screen → pause, then play | `remote command reached Rust: pause` / `play` |
| 4 | Call interrupts and resumes | call the phone, hang up | `interruption began`, `interruption ended (shouldResume): resuming` |
| 5 | AirPlay from Control Center | pick an AirPlay output | `route change …: <name> [AirPlay]`, heartbeats continue |
