# Shelfcast — audiobook playback on Sonos (research, 2026-10-24)

## 1. Formats, MIME, long files, seeking
- Sonos supports AAC-LC/HE-AAC/HEv2 in .m4a/.mp4/.aac (audio/mp4, audio/aac), FLAC (audio/flac), MP3 (audio/mpeg etc.), Ogg Vorbis (application/ogg), WMA, over HTTP/HTTPS. Fragmented MP4 supported. https://docs.sonos.com/docs/supported-audio-formats
- **.m4b**: no listing for the extension. It's the same AAC-in-MP4 container, so serving it as `audio/mp4` should work. **UNVERIFIED** on S2 UPnP. One integration reports "M4B files have very spotty coverage; MP3s with proper mime-type have full support": https://github.com/jmt-gh/audiobookshelf-sonos . Sonos community thread: large (200MB+) m4b via Plex fails after many 6144-byte ranged GETs, which is consistent with moov-at-end / small-chunk layout: https://en.community.sonos.com/controllers-and-music-services-228995/issues-with-plex-playing-audiobooks-6878322
- **Opus**: not in the supported list, so treat it as unsupported (needs a transcode). Same doc URL as above.
- **MIME vs extension**: the Sonos docs say the stream type "is determined by the content type of the data at the streaming URI" https://docs.sonos.com/docs/playback-on-sonos ; HLS is detected by media type https://docs.sonos.com/docs/http-live-streaming-hls . With UPnP, a wrong DIDL protocolInfo/MIME gives error 714 "Illegal MIME-Type": https://community.home-assistant.io/t/sonos-upnp-error-714-received-illegal-mime-type/763320/4 . Whether the player also sniffs the URL extension over UPnP is **UNVERIFIED**. Recommendation: send the correct Content-Type, the correct protocolInfo in DIDL, and a URL that ends in the real extension.
- **MP4 layout**: moov must come before mdat, with large chunks (small stco relative to stsz) https://docs.sonos.com/docs/troubleshooting . Put metadata and cover art at the front or omit them, and keep data contiguous https://docs.sonos.com/docs/streaming-basics
- **Seeking**: the server must support Range/206, return 416 when the range is invalid, send an accurate Content-Length, and answer HEAD. Without these, seeking fails https://docs.sonos.com/docs/streaming-basics . VBR MP3 needs a Xing TOC for scrubbing https://docs.sonos.com/docs/supported-audio-formats . URLs must stay valid for the whole track (troubleshooting doc).
- **Hard file-size or duration limit**: none documented. **UNVERIFIED**. Deep seeks into 10h+ files are **UNVERIFIED**. Expect them to work only for faststart MP4 or CBR/Xing MP3 with Range support.

## 2. Multi-file over UPnP (AVTransport)
- AddURIToQueue / AddMultipleURIsToQueue (returns FirstTrackNumberEnqueued/NumTracksAdded); GetPositionInfo returns `Track` (1-based queue index), `RelTime`, `TrackDuration`, `TrackURI`; Seek `Unit=TRACK_NR` (1-based) then `REL_TIME hh:mm:ss`, or `TIME_DELTA`; LastChange events carry `CurrentTrack`, `CurrentTrackDuration`, `CurrentTrackURI`, `CurrentTrackMetaData`. https://sonos.svrooij.io/services/av-transport (md: https://github.com/svrooij/sonos-api-docs/blob/main/docs/services/av-transport.md)
- So the bridge can map (Track, RelTime) → ABS file index plus offset, then add that file's startOffset to get whole-book time. Before playback, set the AVTransport URI to the queue (`x-rincon-queue:RINCON_xxx#0`).
- Pitfalls:
  - The Sonos queue is shared and can be edited by users, so match on TrackURI rather than trusting the index. [INFERENCE]
  - RelTime has 1-second resolution (hh:mm:ss format, per the svrooij doc).
  - Seek REL_TIME only works within the current track; a cross-file seek takes two calls (TRACK_NR, then REL_TIME).
  - A per-call cap for AddMultipleURIsToQueue (commonly cited as 16) is **UNVERIFIED**.

## 3. Cloud Queue
- Each ABS audio file becomes one cloud-queue item with its own itemId. The player fetches GET /itemWindow?itemId&previousWindowSize&upcomingWindowSize&reason… https://docs.sonos.com/reference/itemwindow . Resume uses loadCloudQueue/playback with itemId + positionMillis https://docs.sonos.com/reference/playback-playbackstatus , and the item/windowPlayhead objects carry positionMillis https://docs.sonos.com/docs/playback-objects , https://docs.sonos.com/docs/add-programmed-radio . Seek within an item: playbackSession seek takes an itemId https://docs.sonos.com/reference/playbacksession-seek-sessionid
- POST /timePlayed reports are per item (update/final), with `positionMillis` relative to that item, plus durationPlayedMillis, positionMillisAtSegmentStart and timeSincePlaybackMillis https://docs.sonos.com/reference/timeplayed , https://docs.sonos.com/docs/add-reporting , https://docs.sonos.com/reference/v23-item-attributes
- Whole-book time = startOffset(item's file) + positionMillis/1000 [INFERENCE]. The playbackStatus event also gives itemId + positionMillis.

## 4. ABS URLs, auth, chapters
- Raw file: `GET /api/items/:id/file/:ino` (also `/file/:ino/download`), routes at https://github.com/advplyr/audiobookshelf/blob/master/server/routers/ApiRouter.js (L124-126). Book.getTracklist builds `contentUrl=/api/items/${id}/file/${ino}` and a cumulative `startOffset` (sum of previous durations, includedAudioFiles order) https://github.com/advplyr/audiobookshelf/blob/master/server/models/Book.js
- Auth: the JWT strategy reads a Bearer header OR `?token=` (`ExtractJwt.fromUrlQueryParameter('token')`) https://github.com/advplyr/audiobookshelf/blob/master/server/Auth.js , so `?token=` works on /api/* file URLs. How API-key vs access-token expiry interacts with long-lived URLs is **UNVERIFIED**. Access tokens have a configurable expiry (TokenManager).
- Session-based alternative: `POST /api/items/:id/play` (body: forceDirectPlay/forceTranscode/supportedMimeTypes) returns a session with audioTracks. The public route `GET /public/session/:sessionId/track/:index` needs no token https://github.com/advplyr/audiobookshelf/blob/master/server/routers/PublicRouter.js , https://github.com/advplyr/audiobookshelf/blob/master/server/managers/PlaybackSessionManager.js
- HLS transcode: `/hls/:streamId/:file` (.m3u8/.ts). It has no auth middleware; the stream id acts as the secret https://github.com/advplyr/audiobookshelf/blob/master/server/routers/HlsRouter.js , mounted in https://github.com/advplyr/audiobookshelf/blob/master/server/Server.js
- Chapters: `media.chapters[]` {id, start, end, title} in seconds of whole-book time; `audioFiles[]` include ino, index, duration, mimeType, and per-file chapters. API docs: https://api.audiobookshelf.org/

## 5. HLS on Sonos
- Supported: AAC-LC/HE-AAC in MPEG-TS/MP4, m3u8, MIME application/x-mpegURL / application/vnd.apple.mpegurl / audio/mpegurl, "track" type for on-demand, 10–15s segments recommended; **MP3 under HLS is not supported** https://docs.sonos.com/docs/http-live-streaming-hls . The doc describes SMAPI/cloud usage, so Cloud Queue mediaUrl with HLS is plausible. HLS via raw UPnP SetAVTransportURI is **UNVERIFIED**.
- ABS's transcode stream is tied to an ABS playback session and starts at a set offset. Whether its segment codec is AAC (needed for Sonos) is **UNVERIFIED**. Prefer direct play.

## 6. Server identity
- `GET /status` (unauthenticated) returns {app:'audiobookshelf', serverVersion, isInit, language, authMethods, authFormData}. It has **no unique server id** https://github.com/advplyr/audiobookshelf/blob/master/server/Server.js . /ping returns {success:true}.
- No stable server UUID endpoint found (**UNVERIFIED** absence). Key accounts on normalized base URL + user.id from `GET /api/me` or `/api/authorize` [INFERENCE].
