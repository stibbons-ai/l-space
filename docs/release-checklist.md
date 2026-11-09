# Release hardware checklist

Run before each release, on real Sonos speakers and an iPhone with the App, against a Bridge running the release's image as described in the [README](../README.md#run-with-docker). Automated tests cover the Bridge against a real Audiobookshelf; only this checklist covers Sonos and iOS (PRD #1, "Testing Decisions"). Terms are those of [GLOSSARY.md](../GLOSSARY.md).

Copy this list into the release's pull request or issue and tick each item; a failed item blocks the release until it is fixed or recorded as a known issue.

## Setup

- [ ] The Bridge runs the release's image, and the UPnP setting in the Sonos app is enabled (Account → Privacy & Security → Connection Security → UPnP).
- [ ] The App is logged in to a Library Server holding at least one podcast episode and one long audiobook (several hours, several chapters, ideally several files).
- [ ] The Library Server shows the Progress each step below expects; check it in Audiobookshelf's web UI, not only in the App.

## Handoff then force-quit

1. Start a Handoff of an Item with Progress to a Group.
2. Force-quit the App as soon as the Group starts playing.
3. Wait at least two minutes.
4. Reopen the App.

- [ ] The Group starts at the Item's Progress and keeps playing with the App closed.
- [ ] The Item's Progress on the Library Server keeps advancing (written about every 30 seconds).
- [ ] The reopened App shows the Group playing the Item at its current position.

## Leaving the house

1. Start a Handoff of an Item to a Group; someone stays at home with it, or another device keeps the Sonos app open on that Group.
2. With Tailscale on, take the phone out of the house, off the home Wi-Fi, for at least five minutes.
3. Away from home, open the App at the same Bridge URL as at home.
4. Come back.

- [ ] The person at home (or the Sonos app) reports no pause, stop or restart while the phone was away.
- [ ] On the Library Server, the Item's Progress kept advancing while the phone was away, in steps of about 30 seconds.
- [ ] Away from home, the App reaches the Bridge at the same URL and shows the Group playing the Item.

## Both kinds of Sleep timer

Kind one pauses at the end of the current chapter (audiobook) or episode (podcast); kind two after a fixed number of minutes.

1. Start a Handoff of an audiobook mid-chapter, set a Sleep timer for the end of the chapter, and force-quit the App.
2. Start a Handoff of a podcast episode near its end, set a Sleep timer for the end of the episode, and force-quit the App.
3. Start a Handoff, set a 15-minute Sleep timer, force-quit the App and lock the phone.
4. Start a Handoff and set a 30-minute Sleep timer; force-quit the App, reopen it, extend the timer, then cancel it.
5. Start Local playback of an audiobook mid-chapter, set a Sleep timer for the end of the chapter, and lock the phone (the App stays running); then start Local playback with a 15-minute Sleep timer and lock the phone.

- [ ] Steps 1–3: each Group pauses at the chapter's end, at the episode's end, or after 15 minutes, with the App closed.
- [ ] Steps 1–3 and 5: each pause writes Progress at the paused position on the Library Server.
- [ ] Step 2: when the end-of-episode timer fires, playback has reached the episode's end, so the episode is Finished on the Library Server, and the Group stays paused or stopped: nothing else starts.
- [ ] Step 4: the reopened App shows the running Sleep timer with its time left; extending moves it later, and after cancelling, the Group plays past the original time.
- [ ] Step 5: Local playback pauses at the chapter's end and after 15 minutes, with the phone locked.

## Takeover from the Sonos app

1. Start a Handoff to a Group. Capture its stream URL from a local packet capture of the player's HTTP GET on the Bridge listen port (the path is a capability: keep the capture private and delete it afterward).
2. In the Sonos app, play something else on that Group (for example a radio station), or regroup its speakers.

- [ ] The Handoff ends: the App no longer shows the Item playing on that Group.
- [ ] The Item's unfinished Progress is the last reliable old-stream position (normally within the 30-second cadence while playing; failed reads can widen the gap), never the new source's position. Repeat paused. In a private local capture of SUBSCRIBE responses, record the granted TIMEOUT and confirm renewal before expiry (normally half the grant); repeat the Takeover after the original lease would have expired.
- [ ] The Group plays what the Sonos app started; the Bridge doesn't take it back.
- [ ] Repeat for membership changes and coordinator changes; unrelated Groups keep playing and continue cadence Progress writes, and other users' Progress remains unchanged. Fetch the captured old stream URL after Takeover: `404`.
- [ ] After each App pause/resume, seek and volume change, the same Handoff remains active and has correct Progress. Replace it from the App: the replacement Handoff stays active. A concurrent external source change still ends the affected Handoff.

## A deep seek in a long audiobook

1. Start a Handoff of a long audiobook from its beginning.
2. Seek near the end of the book (several hours in, in a later file if it has several), then use previous and next chapter.
3. Repeat the seek in Local playback.

- [ ] Playback resumes within a few seconds at the chosen position, in the right file, with the right chapter shown.
- [ ] Progress on the Library Server is the position in the whole book, right after each seek.

## Local playback while locked, with a phone call and with AirPlay

1. Start Local playback of an Item; lock the phone.
2. While it plays locked, use the lock screen's play/pause, 15 seconds back and 30 seconds forward.
3. Receive a phone call; end it.
4. Route the phone's audio to an AirPlay speaker through iOS, then back to the phone.

- [ ] Playback continues with the phone locked, and the lock screen shows the Item's title and artwork.
- [ ] The lock-screen controls work, and Progress follows them on the Library Server.
- [ ] The call pauses playback, and it resumes when the call ends.
- [ ] Playback moves to the AirPlay speaker and back without restarting the Item or losing its position.
