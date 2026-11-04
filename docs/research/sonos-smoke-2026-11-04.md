# Real Sonos UPnP smoke test — 2026-11-04

Issue: [#3](https://github.com/stibbons-ai/l-space/issues/3).

## Conclusion

The supplied AAC-LC M4B and representative MP3 podcast play unchanged over plain HTTP on the tested Sonos Beam Group. Queue playback also provides title, metadata, artwork and position in the Sonos iOS app. Direct SetAVTransportURI playback provides audio and UPnP position reporting, but the app shows artwork with “Keine Inhalte” and no elapsed time or duration, even after restarting the app.

No repackaging was required for the tested M4B, despite its index being at the end. This does not establish universal M4B compatibility, a maximum duration/file size, or Opus support. Prefer the queue path for the next playback implementation; direct-URI app display differed in this experiment, but its root cause was not diagnosed.

## Setup and evidence

- Actual hardware: Sonos Beam (S14) with bonded surrounds, room “Daniels Zimmer”; not grouped with other rooms during the run.
- Device software: `97.2-81220`, display version `27.1`, S2.
- Plain HTTP on the same LAN; no Library Server authentication, redirects or TLS in the media path.
- Throwaway Bun HTTP server with an explicit media allowlist. Verified HEAD/200, byte Range/206 with accurate Content-Range and Content-Length, and out-of-bounds Range/416. M4B served as `audio/mp4`, MP3 as `audio/mpeg`, cover as `image/jpeg`.
- SOAP actions: SetAVTransportURI, Play, Seek (REL_TIME and TRACK_NR), AddURIToQueue, GetPositionInfo and GetTransportInfo. Queue transport URI: `x-rincon-queue:<coordinator UUID>#0`.
- Playback volume temporarily changed from 51 to Daniel's requested 25.
- Audible behavior and Sonos iOS display confirmed by Daniel. Screenshots directly show the direct-URI “Keine Inhalte” state and the queue title/elapsed-time state. Private screenshots/media remain outside tracked research artifacts.
- Device responses and HTTP request logs were observed live. A successful SOAP action alone was not treated as proof of audible seeking.

## Media inventory

All 132 audio files in the supplied folder were inspected with ffprobe, without errors:

| Supplied media | Inventory | Properties |
|---|---|---|
| `audio-book.m4b` | 1 file | AAC-LC, 44.1 kHz stereo; 404,067,377 bytes; 25,454.142 seconds (7:04:14) |
| Podcast MP3 files | 131 files | All MP3, 44.1 kHz stereo; one representative exercised on hardware |

The representative podcast was “Der Thronfolger (mit Melika Foroutan)”: 45,961,504 bytes, 3,282.625306 seconds (54:42).

The M4B top-level atoms were `ftyp` at 0, `free` at 28, `mdat` at 36, and `moov` at byte 399,610,671 (4,456,706 bytes). Sonos fetched the trailing index with HTTP ranges and subsequently requested audio from later offsets. No faststart remux or transcode was applied.

## Results

| Scenario | Playback and seeking | Position reporting | Sonos iOS display |
|---|---|---|---|
| Original M4B, direct URI | Clean audible start. Six-hour seek accepted and playback remained clear; exact audible destination initially not independently confirmed. Backward seek from about 6:03 to 1:00 audibly confirmed, clear afterward. | Duration 7:04:14; position advanced after seeks, including 1:00:15. | Supplied external artwork visible; “Keine Inhalte”; no title, elapsed time or duration. |
| Actual podcast MP3, direct URI | Audible start; jump from about 2:36 to 40:00 audibly confirmed. | Duration 54:42; position advanced to 40:16. | Artwork only, “Keine Inhalte”, no position. App kill/restart did not fix it. |
| Two-file MP3 book, queue | Audible chapter 1. Seek to 4:30 near its end; automatic transition into chapter 2 confirmed. Within chapter 2, seek to 15:00 audibly confirmed. | Track changed 1 to 2 with matching TrackURI and relative position reset; chapter 2 reported 0:00:22 before seek, then 0:15:00. | Chapter title, author/book metadata, elapsed/remaining time and artwork visible. Screenshot: “001 - Etymology”, Herman Melville, 0:20 elapsed / -4:17 remaining. |
| Same original M4B, queue | Selected queue track 3 using TRACK_NR, then REL_TIME 6:00:00. Daniel confirmed audiobook audio and functioning app position. | Track 3, correct M4B URL, duration 7:04:14; advanced to 6:01:21. | Daniel confirmed audiobook artwork, title, metadata and position all present. |

### Metadata caveats

The test DIDL included a custom title, `object.item.audioItem.musicTrack`, matching `res protocolInfo`, and an external `upnp:albumArtURI` pointing to the supplied podcast cover. That same podcast cover was deliberately supplied for the early M4B test and both MP3 book chapters; its appearance was not evidence of stale artwork.

External DIDL artwork display was verified by its appearance with unrelated audiobook/chapter audio. Exact DIDL title fidelity was **not** verified: Sonos returned embedded M4B metadata, and the queue app showed the chapter's embedded title “001 - Etymology”, not our custom test title. For queued M4B, Daniel reported that the audiobook's own artwork appeared despite the test supplying the podcast image; artwork precedence was not isolated.

### Queue sample sources

Two consecutive public-domain LibriVox sections, not a complete-book import:

- [Moby Dick, version 2 — official catalogue](https://librivox.org/moby-dick-or-the-whale-version-2-by-herman-melville/).
- [001 — Etymology](https://archive.org/download/mobydick2_2511_librivox/mobydickorthewhale_001_melville_128kb.mp3): 4,467,687 bytes; ffprobe duration 278.894875 seconds.
- [002 — Extracts](https://archive.org/download/mobydick2_2511_librivox/mobydickorthewhale_002_melville_128kb.mp3): 22,815,677 bytes; ffprobe duration 1,425.658776 seconds.

Audio downloaded to temporary local storage and served through the same LAN HTTP server. LibriVox declares its recordings public domain in the USA; operators outside the USA must check local rights. No audio is committed or redistributed with this report.

## Repackaging decision and limits

- **Tested AAC-LC M4B:** no repackaging needed for this file on this Group. Trailing `moov` alone did not prevent playback or six-hour seek. Do not make front-index remuxing mandatory solely on this result or prior speculation.
- **Tested MP3 podcast/book files:** no repackaging needed. Queue track selection, within-track seek and automatic chapter advance worked.
- **Opus, other AAC profiles, other podcast formats:** not tested on hardware. No such codecs were present in the supplied folder. Public candidate discovery is not playback evidence or proof of coverage of all personal feeds.
- **Limits:** demonstrated one 404 MB, 7-hour file and six-hour seek. No maximum file-size/duration limit established; 10h+ playback and multi-Group variability remain untested. No claim that all 131 MP3 episodes were played.
- **Title:** useful queue metadata display verified; exact arbitrary DIDL title display remains unverified.
- The original paused Spotify source was replaced during testing and was not restored. At completion playback was stopped, the three test queue entries removed (original queue was empty), and volume 51 restored while stopped. Temporary HTTP serving was shut down.

Issue #3 should remain open if acceptance requires inventorying podcast feeds beyond the supplied folder or verifying arbitrary DIDL title behavior. These findings establish the tested sample behavior, not broader compatibility.
