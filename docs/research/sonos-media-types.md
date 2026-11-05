# Sonos media types for Handoff streams (research 2026-11-05)

The Bridge's stream endpoint (ADR 0010) sets a `Content-Type` Sonos accepts, chosen by the audio file's extension (`sonos::media_type`, `crates/sonos/src/media.rs`). An extension not listed is not playable on a Group (`item_not_playable`).

## Sources

- Sonos developer docs, [Supported audio formats](https://docs.sonos.com/docs/supported-audio-formats) (updated 2026-04-16): AAC-LC/HE-AAC/HEv2-AAC in `.m4a`, `.mp4`, `.aac` as `audio/mp4` or `audio/aac`; FLAC `.flac` as `audio/flac`; MP3 `.mp3` as `audio/mp3`, `audio/mpeg3` or `audio/mpeg`; Ogg Vorbis `.ogg` as `application/ogg`; WMA `.asf`, `.wma` as `audio/wma` or `audio/x-ms-wma`. "You must provide the media type using the proper case".
- Sonos support, [Supported audio formats for Sonos music library](https://support.sonos.com/en-us/article/supported-audio-formats-for-sonos-music-library): also ALAC, AIFF (16-bit) and WAV (16-bit), mono or stereo up to 48 kHz; no media types given.
- Our hardware smoke test, [sonos-smoke-2026-11-04.md](sonos-smoke-2026-11-04.md): AAC-LC M4B and HE-AAC M4A served as `audio/mp4`, MP3 as `audio/mpeg`, played; Ogg Opus served as `audio/ogg` was skipped silently.

## Mapping

| Extension | `Content-Type` | Basis |
|---|---|---|
| `.mp3` | `audio/mpeg` | developer docs; smoke test |
| `.m4a`, `.m4b`, `.mp4` | `audio/mp4` | developer docs (`.m4b` is MP4 too); smoke test |
| `.aac` | `audio/aac` | developer docs |
| `.flac` | `audio/flac` | developer docs |
| `.ogg`, `.oga` | `application/ogg` | developer docs (Ogg Vorbis) |
| `.wma`, `.asf` | `audio/x-ms-wma` | developer docs |
| `.wav` | `audio/wav` | support article lists WAV; the media type is the common one, UNVERIFIED on hardware |
| `.aif`, `.aiff` | `audio/aiff` | support article lists AIFF; the media type is the common one, UNVERIFIED on hardware |
| `.opus`, anything else | none: not playable | smoke test (Opus skipped silently) |

## Open

- An `.ogg` file may hold Opus, which Sonos skips silently; the extension can't tell. Detecting the codec (Audiobookshelf reports one per audio file) belongs with the repackaging slice.
- WAV and AIFF media types are unverified on hardware; the black-box tests serve WAV to `sonos-fake` only.
