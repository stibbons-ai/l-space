# Real Sonos UPnP smoke test — 2026-11-04

Issue: [#3](https://github.com/stibbons-ai/l-space/issues/3).

## Conclusion

The supplied AAC-LC M4B and representative MP3 podcast play unchanged over plain HTTP on the tested Sonos Beam Group. Queue playback also provides title, metadata, artwork and position in the Sonos iOS app. Direct SetAVTransportURI playback provides audio and UPnP position reporting, but the app shows artwork with “Keine Inhalte” and no elapsed time or duration, even after restarting the app.

No repackaging was required for the tested M4B, despite its index being at the end. This does not establish universal M4B compatibility or a maximum duration/file size. Prefer the queue path for the next playback implementation; direct-URI app display differed in this experiment, but its root cause was not diagnosed.

A second run (same day, same Group) added three findings:

- An HE-AAC M4A podcast plays unchanged, including a deep seek.
- **Ogg Opus is silently skipped**: there is no SOAP error, transport status stays `OK`, and the app shows no error. The same audio transcoded to AAC-LC M4A plays and seeks.
- **Sonos ignores DIDL `dc:title`/`dc:creator` for queued HTTP items.** The app title comes only from tags embedded in the file; with no tags it shows “Keine Inhalte”. DIDL `upnp:albumArtURI` is displayed when the file has no embedded picture. Embedded artwork wins over DIDL artwork.

## Setup and evidence

- Actual hardware: Sonos Beam (S14) with bonded surrounds, room “Daniels Zimmer”; not grouped with other rooms during the run.
- Device software: `97.2-81220`, display version `27.1`, S2.
- Plain HTTP on the same LAN; no Library Server authentication, redirects or TLS in the media path.
- Throwaway Bun HTTP server with an explicit media allowlist. Verified HEAD/200, byte Range/206 with accurate Content-Range and Content-Length, and out-of-bounds Range/416. M4B served as `audio/mp4`, MP3 as `audio/mpeg`, cover as `image/jpeg`.
- SOAP actions: SetAVTransportURI, Play, Seek (REL_TIME and TRACK_NR), AddURIToQueue, GetPositionInfo and GetTransportInfo. Queue transport URI: `x-rincon-queue:<coordinator UUID>#0`.
- Playback volume temporarily changed from 51 to Daniel's requested 25.
- Audible behavior and Sonos iOS display confirmed by Daniel. Screenshots directly show the direct-URI “Keine Inhalte” state and the queue title/elapsed-time state. Private screenshots/media remain outside tracked research artifacts.
- Device responses and HTTP request logs were observed live. A successful SOAP action alone was not treated as proof of audible seeking.
- Second run: same room, software and volume procedure. A fresh allowlisted Bun server passed the same HEAD/206/416 checks before speaker use, plus a 404 for directory listing and path traversal. M4A served as `audio/mp4`, Opus as `audio/ogg`. Daniel confirmed audio and app display for every scenario below.

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
| HE-AAC M4A podcast (FS313, 4:38:51), queue | Clean audible start. Seek to REL_TIME 3:30:00 audibly jumped, clear afterward. Sonos fetched the `moov` via HTTP ranges, then requested byte 80,838,656 (76.5% of file), so it did not stream through to the seek target. | Duration 4:38:51; advanced 0:00:02→0:00:11, then 3:30:02→3:30:11. | Embedded title “FS313 Der Tripelpunkt der Metaebene” and embedded Freak Show cover, **not** the DIDL title/whale cover supplied. Progress correct. |
| Ogg Opus podcast (FG100), queue, unchanged | Not played. Sonos made two full GETs of the file within ~1 s, then advanced past it. Opus was the last queue item, so the pointer wrapped to track 1 and played HE-AAC from 0:00 (PlayMode `NORMAL`). | No SOAP or transport error; `CurrentTransportStatus` stayed `OK`. | Daniel: Freak Show restarted from the beginning; no error shown. |
| Same Opus, temporary AAC-LC M4A transcode (96 kb/s, 48 kHz stereo, faststart), queue | Clean audible start; seek to 1:30:00 audibly jumped, clear. | Duration 1:52:16; advanced 1:30:02→1:30:08. | “Keine Inhalte” with progress. ffmpeg's default mapping dropped the Opus stream-level `TITLE` tag, so the transcode had no title tag. |
| Moby Dick 001 MP3, tags stripped (stream copy, no ID3v1/v2, no picture; verified by ffprobe and an MPEG frame sync at byte 0), queue | Clear audible narration. | Duration 0:04:38; advancing. | **“Keine Inhalte”**, no artist, DIDL whale artwork shown (Sonos fetched `/moby.jpg`). The distinctive DIDL title `LSPACE DIDL PROBE 7Q` and creator were not shown. |
| Same stripped MP3, DIDL plus Sonos `<desc id="cdudn">RINCON_AssociatedZPUDN</desc>` | Clear audio. | Advancing. | Same: “Keine Inhalte”, whale artwork, progress. The artwork was not fetched again (likely cached), so it does not show the DIDL art URI was kept. |

### Metadata caveats

The test DIDL included a custom title, `object.item.audioItem.musicTrack`, matching `res protocolInfo`, and an external `upnp:albumArtURI`. In the first run, that same podcast cover was deliberately supplied for the early M4B test and both MP3 book chapters; its appearance was not evidence of stale artwork.

The second run isolated title and artwork:

- **Title/creator:** a queued HTTP item's stored metadata (ContentDirectory `Q:0/N` BrowseMetadata and `GetPositionInfo` TrackMetaData) contains `dc:title` = the URL's last path segment, with no creator and no `albumArtURI`. Our DIDL title, creator and art URI are all discarded, with or without the `cdudn` desc. The app shows embedded tags when present (first-run “001 - Etymology”, FS313 title), otherwise “Keine Inhalte”. Echoed SOAP success is not display proof; Daniel observed every display state listed.
- **Artwork:** for the tag-free MP3, Sonos fetched the DIDL `albumArtURI` at play time and the app showed it. With embedded pictures (FS313 M4A, first-run M4B), the embedded artwork was shown instead of the DIDL image.
- Untested: Sonos-specific URI schemes (for example `x-file-cifs`, `x-sonos-http`) and SMAPI music-service metadata. Direct SetAVTransportURI with DIDL already showed “Keine Inhalte” in the first run.

### Queue sample sources

Two consecutive public-domain LibriVox sections, not a complete-book import:

- [Moby Dick, version 2 — official catalogue](https://librivox.org/moby-dick-or-the-whale-version-2-by-herman-melville/).
- [001 — Etymology](https://archive.org/download/mobydick2_2511_librivox/mobydickorthewhale_001_melville_128kb.mp3): 4,467,687 bytes; ffprobe duration 278.894875 seconds.
- [002 — Extracts](https://archive.org/download/mobydick2_2511_librivox/mobydickorthewhale_002_melville_128kb.mp3): 22,815,677 bytes; ffprobe duration 1,425.658776 seconds.

Audio downloaded to temporary local storage and served through the same LAN HTTP server. LibriVox declares its recordings public domain in the USA; operators outside the USA must check local rights. No audio is committed or redistributed with this report.

## Repackaging decision and limits

- **Tested AAC-LC M4B:** no repackaging needed for this file on this Group. Trailing `moov` alone did not prevent playback or six-hour seek. Do not make front-index remuxing mandatory solely on this result or prior speculation.
- **Tested MP3 podcast/book files:** no repackaging needed. Queue track selection, within-track seek and automatic chapter advance worked.
- **HE-AAC M4A:** no repackaging needed (one 4h39m file, deep seek).
- **Ogg Opus:** unsupported as served (`audio/ogg`, unchanged), and it fails **silently**: no SOAP fault, `OK` transport status, no app error, the queue just moves on. The Bridge must detect Opus from the codec before enqueueing and transcode to AAC-LC M4A; it cannot rely on a transport error. A temporary AAC-LC transcode played and seeked correctly. Only this one Opus file and MIME type were tried. Other AAC profiles, Vorbis, FLAC and other podcast formats remain untested.
- **Display title:** because Sonos ignores DIDL titles, any title the Bridge wants shown must be embedded in the served bytes (ID3/MP4 tags). Transcodes must carry tags across explicitly (e.g. map the Opus stream-level tags); the default ffmpeg mapping loses them.
- **Limits:** demonstrated one 404 MB, 7-hour file and six-hour seek. No maximum file-size/duration limit established; 10h+ playback and multi-Group variability remain untested. No claim that all 131 MP3 episodes were played.
- **Title:** queue display of embedded titles verified. Arbitrary DIDL titles are **not** displayed (verified on hardware); DIDL artwork is displayed only when the file has no embedded picture.
- The original paused Spotify source was replaced during the first run and was not restored. At completion of both runs, playback was stopped, only the test queue entries were removed (3, then 5, each matched by URI; the original queue was empty), and volume 51 was restored while stopped. Temporary HTTP serving was shut down.

### Second-run media sources

Downloaded to untracked local storage; no audio committed or redistributed. Full provenance and SHA256 values are in the local download manifest.

- [Freak Show FS313](https://freakshow.fm/fs313-der-tripelpunkt-der-metaebene) — Metaebene Personal Media, Tim Pritlove ([file](https://media.metaebene.me/media/freakshow/fs313-der-tripelpunkt-der-metaebene.m4a)); 105,637,832 bytes, HE-AAC 44.1 kHz stereo. Official free download, no explicit CC grant found: private testing only.
- [Forschergeist FG100 — Stellarator](https://forschergeist.de/podcast/fg100-stellarator/) — Metaebene, Tim Pritlove ([file](https://media.metaebene.me/media/forschergeist/episodes/fg100-stellarator.opus)), [CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/); 55,573,464 bytes, Opus 48 kHz stereo. The AAC transcode was a temporary local adaptation, deleted after testing.
- Moby Dick 001 (source above) and its [cover](https://archive.org/download/mobydick2_2511_librivox/mobydick_2511.jpg); the tag-stripped copy was a temporary derivative. Cover-specific rights not audited.

Issue #3 should remain open if acceptance requires inventorying podcast feeds beyond the supplied folder or arbitrary DIDL title display. The latter is now shown to be unavailable via queued HTTP DIDL, so it needs a design decision: embed tags in the served bytes or accept file-derived titles. These findings establish the tested sample behavior, not broader compatibility.
