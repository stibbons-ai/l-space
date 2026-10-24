# Media streams through the Bridge

Neither Groups nor the App fetch media from Library Servers. Each Handoff or Local playback session gets a URL on the Bridge, `/stream/<random id>/<file>`, which only works while that session is active. The Bridge forwards byte-range requests to the Library Server with its own credentials and sets a `Content-Type` Sonos accepts. This keeps Audiobookshelf credentials out of speaker queues and gives repackaged files (such as `.m4b` with the index moved to the front) one place to be served from. Audio passes through the Bridge, which is negligible on a LAN, and playback depends on the Bridge being up, as Progress and the Sleep timer already do. Away from home, Local playback needs the Bridge reachable over the household VPN.

## Considered Options

- **Access token in the URL**: Audiobookshelf access tokens expire after one hour and are checked on each request, so seeks, reconnects and file changes would fail after an hour.
- **API key in the URL**: a permanent, full-permission credential would sit in every speaker's queue.
