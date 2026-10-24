# Shelfcast

Self-hosted playback of Audiobookshelf podcasts and audiobooks, on Sonos or on the phone: pick something, send it to a speaker, and walk away.

## Language

**Bridge**:
The always-on Shelfcast server in the household. It is the App's only backend and the only part of Shelfcast that talks to Sonos.
_Avoid_: backend, server, hub

**App**:
The Shelfcast client on the phone. It is a remote for the Bridge and can also play Items itself (Local playback).
_Avoid_: client

**Library Server**:
An Audiobookshelf server whose Items Shelfcast plays. One Bridge can serve several.
_Avoid_: instance, ABS server, backend

**Library**:
A collection of either audiobooks or podcasts on a Library Server. A Library Server can have several of each kind, and the App lists them all.
_Avoid_: collection, shelf

**Item**:
One audiobook or one podcast episode in a Library. It is the unit of a Handoff, of Progress, and of being Finished.
_Avoid_: track, media, title, episode (when either kind is meant)

**Group**:
One or more Sonos players playing in sync; the target of playback. A single speaker is a Group of one.
_Avoid_: speaker, room, zone

**Handoff**:
Starting an Item on a Group through the Bridge, either fresh or by moving Local playback there at its current position. Afterwards, playback no longer depends on the App. Starting Local playback from a Group's position ("Play here") ends the Handoff.
_Avoid_: cast, transfer, AirPlay

**Local playback**:
Playing an Item in the App on the phone, including on any AirPlay or Bluetooth output iOS routes it to. The phone does the streaming, so the App must stay alive.
_Avoid_: phone mode, AirPlay mode

**Progress**:
How far an Audiobookshelf user is into an Item, stored on its Library Server as the single source of truth. During a Handoff, Progress is recorded for the user who started it, whoever controls the Group afterwards.
_Avoid_: position, play state, resume point

**Finished**:
An Item the listener has heard to the end or marked as done, as recorded on its Library Server.
_Avoid_: heard, played, completed

**Sleep timer**:
A point at which current playback, a Handoff or Local playback, is paused: the end of the current chapter (audiobook), the end of the episode (podcast), or a fixed number of minutes from now.
_Avoid_: stop timer, auto-off

**Playback speed**:
How fast an Item plays relative to normal, remembered per podcast and once per audiobook Library. Groups always play at normal speed.
_Avoid_: rate, tempo

**Takeover**:
Something other than the Bridge changes what a Group is playing (another app, a new source, regrouping), which ends the current Handoff.
_Avoid_: eviction, interruption
