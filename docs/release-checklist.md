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

- [ ] The Group starts at the Item's Progress and keeps playing with the App closed.
- [ ] The Item's Progress on the Library Server keeps advancing (written about every 30 seconds).
- [ ] Reopening the App shows the Group playing the Item at its current position.

## Leaving the house

1. Start a Handoff to a Group.
2. Take the phone out of the house, off the home Wi-Fi and the household's Tailscale, for at least five minutes.
3. Come back; reopen the App.

- [ ] The Group plays on, without a gap, the whole time the phone is away.
- [ ] Progress keeps advancing on the Library Server while the phone is away.
- [ ] Away from home, with Tailscale on, the App reaches the Bridge at the same URL and shows the Group playing.

## Both kinds of Sleep timer

1. Start a Handoff of an audiobook mid-chapter and set a Sleep timer for the end of the chapter; force-quit the App.
2. Start a Handoff of a podcast episode and set a Sleep timer for the end of the episode; force-quit the App.
3. Start a Handoff and set a 15-minute Sleep timer; force-quit the App and lock the phone.
4. Repeat one of the above in Local playback.

- [ ] Each Group pauses at the chapter's end, the episode's end, or after 15 minutes, with the App closed.
- [ ] Each pause writes Progress at the paused position on the Library Server; the podcast episode is Finished only if playback reached its end.
- [ ] Before it fires, reopening the App shows the running Sleep timer, and extending and cancelling it work.
- [ ] In Local playback the Sleep timer pauses the phone's playback the same way.

## Takeover from the Sonos app

1. Start a Handoff to a Group.
2. In the Sonos app, play something else on that Group (for example a radio station), or regroup its speakers.

- [ ] The Handoff ends: the App no longer shows the Item playing on that Group.
- [ ] The Item's Progress on the Library Server is the position at the moment of the Takeover.
- [ ] The Group plays what the Sonos app started; the Bridge doesn't take it back.

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
