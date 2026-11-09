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

## An audiobook in several files on a Group

Use an audiobook in at least three files (MP3 or M4A/M4B, as Audiobookshelf lists it), with chapters, and give the Group a queue of its own in the Sonos app first.

1. In Audiobookshelf, set the book's Progress to the middle of its second (or a later) file; start a Handoff of it to a Group.
2. Let it play across the end of a file into the next; keep the App closed for a minute after the change.
3. Under 15 seconds into the second or a later file, use "15 s back" from the App.
4. Use "Previous chapter" more than 3 seconds into a chapter (restart it), again at once (the chapter before), then "Next chapter" twice (each following chapter's start). Also try Previous in the first chapter (its start), and Next in the last chapter (nothing).
5. Start the book about a minute before its end and let it play to the end of the last file. Record the queue's position after stopping in `docs/research/`.

- [ ] Step 1: the Group starts in the right file at the right offset (the Sonos app shows that file and its elapsed time), within the Bridge's 14-second start budget; its play call has 4 seconds for up to 16 files plus 1 second per additional batch of at most 16. The queue holds the book's files in order, and what it held before is gone.
- [ ] Step 2: the Group moves on to the next file by itself without a gap the listener notices, and the Item's Progress on the Library Server keeps advancing in the whole book (past the first file's length), not back at the start of the file.
- [ ] Step 3: playback lands near the end of the file before, and Progress is that position in the whole book.
- [ ] Step 4: each chapter skip lands at the chapter's start, in the right file; "Next chapter" in the last chapter does nothing; Progress follows each skip.
- [ ] Step 5: the Group stops at the end of the last file, the book is Finished on the Library Server, and the App no longer shows a Handoff on the Group. Note where the Sonos app shows the queue after the stop (the last file or back at the first): the Bridge doesn't rely on it.
- [ ] Start a book of 30+ files; record the measured start time and file count in `docs/research/`. Confirm batched `AddMultipleURIsToQueue` works on hardware (the 16-URI cap follows SoCo, not a hardware measurement); the play limit is `4 + ceil(files / 16) - 1` seconds, still within the Bridge's 14-second total and the App's 15.

## Local playback while locked, with a phone call and with AirPlay

1. Start Local playback of an Item; lock the phone.
2. While it plays locked, use the lock screen's play/pause, 15 seconds back and 30 seconds forward.
3. Receive a phone call; end it.
4. Route the phone's audio to an AirPlay speaker through iOS, then back to the phone.

- [ ] Playback continues with the phone locked, and the lock screen shows the Item's title and artwork.
- [ ] The lock-screen controls work, and Progress follows them on the Library Server.
- [ ] The call pauses playback, and it resumes when the call ends.
- [ ] Playback moves to the AirPlay speaker and back without restarting the Item or losing its position.
