# One Bridge serves many Library Servers, and is the App's only backend

A household runs one Bridge, and the App switches between Library Servers through it. Two Bridges on one LAN would compete for the same Groups. The App never talks to a Library Server directly: logging in, browsing and playback all go through the Bridge. The Bridge already has to hold each user's Audiobookshelf token so it can write Progress after the App is gone, so this adds no new kind of state. Audiobookshelf exposes no stable server id, so the Bridge identifies a Library Server by its normalized base URL.

## Consequences

- Only the Bridge needs to reach the Library Servers. Groups and the App fetch media from the Bridge (ADR 0010).
