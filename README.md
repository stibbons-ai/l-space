# L-Space

Self-hosted playback of Audiobookshelf podcasts and audiobooks, on Sonos or on the phone. See [GLOSSARY.md](GLOSSARY.md) for the terms and [docs/adr](docs/adr) for the decisions.

## Build and test

Toolchain and tasks come from [mise](https://mise.jdx.dev). In a fresh clone run `mise trust` (mise ignores an untrusted `mise.toml`) and `mise install`, then

| Task | Does |
|---|---|
| `mise run build` | builds every crate and target |
| `mise run test` | runs every test, including the black-box Bridge tests and the App's flow tests |
| `mise run lint` | clippy with warnings denied, and `cargo fmt --check` |
| `mise run check` | build, lint and test |
| `mise run image` | builds the Clacks image for this machine's platform and smoke-tests it ([scripts/smoke-image.sh](scripts/smoke-image.sh)) |

CI runs `mise run check` on every pull request and every push to `main` ([check.yml](.github/workflows/check.yml)).

The black-box Bridge tests start a real Audiobookshelf in a container ([testcontainers](https://crates.io/crates/testcontainers)), so `mise run test` needs a running Docker; the first run pulls the Audiobookshelf image. The App's flow tests (`crates/app/tests`) drive the App's state and actions, without rendering, against a real Bridge and Audiobookshelf; they start the `bridge` binary from the same target directory, so run them through `mise run test` (or `cargo build -p bridge` before `cargo test -p app`). They run apart from the rest of the workspace: built together with the Bridge, `api` gets its `server` feature, and the App's server function calls would run in-process instead of over HTTP.

Crates live in `crates/`: `api` (the App API contract), `bridge` (the Bridge binary), `app` (the App), `abs` (the Audiobookshelf client), `sonos` (Sonos control behind one trait, over local UPnP), `clock` (the time the Bridge and `sonos-fake` share, which tests advance by hand), `store` (the storage traits and their contract test suite), `store-sqlite` (their SQLite implementation), `test-support` (the test harness the Bridge's and the App's tests share) and `sonos-fake` (a fake Sonos household on loopback for those tests).

## Build and run the App

The App (shipped as Ook; its screens call the Bridge Clacks) is a [Dioxus](https://dioxuslabs.com) 0.8 mobile app (ADR 0007), built with Dioxus's CLI `dx`, which `crates/app/mise.toml` pins to the workspace's `dioxus` version (0.8.0-alpha.3). It also points `DEVELOPER_DIR` at `/Applications/Xcode.app/Contents/Developer`, so iOS builds use Xcode without `xcode-select -s`. Run `dx` from `crates/app` through mise, which also shadows Homebrew's unrelated `dx` (Deno):

```sh
cd crates/app
mise trust && mise install                  # once: Rust's iOS targets and dx
mise exec -- dx serve --ios                 # iOS Simulator: builds, installs and launches on the booted simulator
mise exec -- dx serve --ios --release --device "<iPhone name>" \
  --codesign --apple-team-id "Apple Development: <Name> (<TEAMID>)"   # a phone
```

For a phone, set it up as in `spikes/ios-audio/README.md` ("One-time device setup"), but create the provisioning profile for the App's bundle identifier `ai.stibbons.ook`, not the spike's.

The bundle identifier is `ai.stibbons.ook`. On first launch the App asks for the Bridge's address, for example `http://192.168.1.10:8080` (the Simulator reaches a Bridge on the same Mac at `http://127.0.0.1:8080`); the Bridge answers with its Library Servers, and the App remembers the address. An address that redirects to a Bridge while keeping the path it was asked for (say a reverse proxy redirecting `http://` to `https://`) is remembered as the address it redirects to; one that redirects anywhere else (a login page, or a bare `https://host/`) is rejected as not a Bridge, one whose redirect leads nowhere reachable as unreachable, and a redirect from `https://` to `http://` is never followed. The App's calls to the Bridge never follow a redirect, so the session token goes only to the remembered address; a remembered address that starts redirecting counts as the Bridge being unreachable, and the session stays. Then a person picks a Library Server and enters their Audiobookshelf API key (see [Log in](#log-in)). The App keeps the address, its sessions and the language choice in `Application Support/ai.stibbons.ook/app.json` inside its sandbox, readable by its owner only; the API key is not kept. A file the App can't read is moved aside (to the first free `app.json.bad`, `app.json.bad.2`, …), and the App starts afresh and says so; if it can't even move the file, it leaves it untouched, saves nothing during that launch, and says that instead. A session lasts across launches until logout, or until the Bridge answers `api_key_rejected` at launch, or `session_invalid` while it still lists the session's Library Server; then the App forgets it and shows Login with the reason. A session whose Library Server the operator removed from the configuration stays, and the App says so: it may work again once the operator re-adds the Library Server (a session that also ended in the meantime is forgotten at the next launch after that). Any other failure at launch (the Bridge or the Library Server unreachable, also for the listing after `session_invalid`, a call taking longer than 15 seconds, a proxy or captive portal answering instead of the Bridge) keeps the session. Logout also forgets the session when the Bridge can't be reached; if the App can't save that, it says so.

The App holds at most one session per Library Server and uses the selected one: every call sends its token, and a launch checks only that one; another is checked when the person switches to it. Settings names the selected Library Server, switches to any other the person is logged in to without a key (showing its Home afresh), and logs in to another configured Library Server; logging in again where there is a session already replaces it. Logout, a rejected key or an ended session forgets only that Library Server's session; Login then gives the reason, if any, and offers the remaining ones to switch to, never another server's data on its own. The address of the Bridge can change only while no session is stored. An `app.json` from an App that held one session opens logged in on that Library Server.

Logged in (after login, on a launch whose session the Bridge confirms, and after switching Library Server), the App shows Home: "Continue listening", the audiobooks and episodes the user has Progress in past the start and hasn't Finished, the one whose Progress changed last first; and "Latest episodes", the newest episodes the user hasn't Finished of every podcast they can see, newest publish date first (the date Audiobookshelf added an episode without one; Audiobookshelf picks each podcast Library's 20 newest unfinished by publish date, so such an episode shows only when its Library has fewer than 20 unfinished episodes with one). Each list holds at most 20 Items from every Library the user can see on the Library Server, only those Audiobookshelf lets the user open (Library access, explicit content and tags alike), and says so when it is empty; an episode in progress can be in both. A row shows the title, the author or podcast, an episode's publish date and the person's Progress, and opens the Item's screen; Back returns to Home. Home is read afresh on landing, when opened from Groups or Settings, and after a mark (see below); Back from a screen opened from Home shows it as it was. When it can't be read, it offers to try again; the notice saying why goes once the screen is left, as on any screen. The Libraries, Groups and Settings are one tap from Home. The App API's `home` reads, with the user's own key, the user and their Progress (`/api/me`; the Progress a row shows comes from there), all of Audiobookshelf's Items in progress (`/api/me/items-in-progress`, asking for as many as the user has Progress records in `/api/me` plus 10; it doesn't apply "hide from continue listening", and the Bridge leaves out an Item in a Library the user may not see) and each podcast Library's newest episodes (`/api/libraries/<id>/recent-episodes`). It then reads the library item of each list's candidates in order (`/api/items/<id>`, each at most once per Home), the ones it leaves out included, until 20 the user may open are found or none are left: one Audiobookshelf refuses the user (not found or forbidden) is left out, and any other failure fails Home. So another user's Progress never shows on a user's Home nor hides anything from it.

The Libraries screen lists every Library the user can see on the Library Server, audiobooks and podcasts alike. A Library lists its audiobooks or podcasts by title. Left out: a book Audiobookshelf counts no audio files for (an ebook alone), and a Library in a shape the Bridge doesn't know (another kind, for example). A podcast lists its episodes newest first, filtered by the person's Progress: unfinished (the default), Finished, or all. An Item (an audiobook or an episode) shows its title, author or podcast, description, duration, publish date for an episode, the person's Progress ("1 h 12 min left", "Finished", "Not started", or "Started" when the duration is unknown) and an audiobook's chapters with their start times; Back returns to where it was opened from. Progress and Finished are always the logged-in user's own, as their Library Server records them; while browsing, the Bridge only reads from Audiobookshelf. Durations and dates follow the App's language (dates as the UTC calendar day).

On an Item's screen, "Mark as finished" (or "Mark as unfinished" once it is Finished) writes that to the Library Server for the logged-in user, with their own API key, and nobody else's Progress changes; the App API answers with the Item as it then is. While the request runs, the screen's controls are disabled. On success the Item's Progress shows the new state at once, and so does the podcast's episode list under its filter when going back, without a refresh; Home, when going back to it, is read again from the Bridge, so a Finished Item has left "Continue listening" (if that read fails without ending the session, Home stays as it was). An Item the user can't see is `not_found`, and nothing is written. When the Library Server can't be reached, the Bridge may have written before reading the Progress back failed, so the App reads the Item, its podcast's episodes and Home again, as far as it can; on any other failure that keeps the session the screen stays as shown. Either way the App says why. The write may also have happened when reading it back ends the session (`api_key_rejected`) or revoking the key fails (`internal`), and a mark whose answer was lost (the App gave up waiting, for example) may still land on the Library Server; opening the Item again shows its state. What the Library Server does with the position is accepted as it is: Audiobookshelf keeps it when marking Finished, sets it back to the start when marking a Finished Item unfinished, and records Progress at the start for an Item the user had none in, whichever way it is marked.

The Groups screen, one tap from Home like Settings, lists every Sonos Group exactly as grouped in the Sonos app, by name: its members (a single speaker is a Group of one; bonded surrounds, subs and Boosts are not shown) and what it is playing, as Playing, Paused, Stopped or Starting… with the track's title and creator when Sonos has them, "Unknown state" when the Group's coordinator answered without a state the Bridge knows, and "Not answering" when the coordinator wasn't found, failed to answer, or answered too late. Each row also shows where the track is ("2:15 / 10:00") and the Group's volume, with the controls of [Control a Group](#control-a-group). Refresh lists them afresh.

Every string inside the App comes from Fluent (`crates/app/locales/en.ftl`, `de.ftl`, embedded in the binary). The Info.plist strings (display name, local network usage text) are the exception and stay English, since dx doesn't generate `InfoPlist.strings`. The App shows German on a German device and English otherwise, unless Settings picks a language.

## Run the Bridge

```sh
(umask 077; openssl rand -base64 32 > bridge_encryption_key)   # once; keep it, see "Encryption key" below
BRIDGE_ENCRYPTION_KEY_FILE=bridge_encryption_key cargo run -p bridge -- --config bridge.toml   # or BRIDGE_CONFIG=bridge.toml
```

```toml
# bridge.toml
listen = "0.0.0.0:8080"           # required; port 0 picks a free port
database = "/var/lib/clacks/bridge.db"   # required; created if missing (its directory must exist), relative to the working directory

[[library_servers]]
url = "https://abs.example.com"   # Audiobookshelf base URL, may include a subpath
name = "Home"                     # optional
```

Each Library Server is identified by its normalized base URL: lowercase scheme and host, no default port, no trailing slash. Only `http` and `https` are accepted; a query, fragment, user name or password is rejected, as are two entries that normalize to the same URL. Any `@` counts as a user name or password and is rejected, so a password with an unencoded `/`, `\`, `?` or `#` can't pass for a host or path.

A configuration that can't be read or parsed, or a bad Library Server URL, stops the Bridge with exit code 1 and one `invalid configuration` error event. For a bad URL the event names the entry and shows only the URL's scheme, host, port and path (a query or fragment as `REDACTED`), or nothing of it when it contains `@`, isn't written as `scheme://…`, can't be parsed or has no host. For a TOML error it gives the line and column (a field missing inside a table points at the table (its header, or the inline table); a missing top-level field has none), never the source line. If the database can't be opened, the Bridge exits with code 1 after one `cannot open the database` error event; if the `listen` address can't be bound, after one `cannot listen` error event.

The database holds the Bridge's state: App sessions, active Handoffs, and the Audiobookshelf API keys users logged in with (a Handoff acts with its starter's). Each key is encrypted (XChaCha20-Poly1305, bound to its Library Server and user) and is looked up by an HMAC keyed by the encryption key, never by a plain hash; session tokens and stream ids are stored only as their SHA-256. On Unix the Bridge also makes the database readable by its owner only, also when it already existed. Logout ends only the session: the key stays stored, and so do keys and sessions of Library Servers removed from the configuration. A database written by a newer Bridge is refused. Upgrading a database from a Bridge that stored keys in plain text deletes those keys and their sessions, so their users log in again. Before that, the Bridge rebuilds the file (`VACUUM`), so that keys the earlier Bridge had deleted don't linger in free pages; if the rebuild fails, nothing is upgraded and the next start tries again. The upgrade's own deletes are overwritten in the file. During the upgrade SQLite's rollback journal briefly holds copies of the old pages; it is deleted, not overwritten, afterwards, and a crash mid-upgrade leaves it in place until the next start rolls it back.

Logs are JSON on stdout, one event per line. `RUST_LOG` adds to the default level `info`: `RUST_LOG=hyper=debug` raises one target and leaves the rest at `info`, and `RUST_LOG=warn` lowers the default. `RUST_LOG=sonos::raw=debug` logs every SSDP datagram and SOAP request and response body the Bridge exchanges with Sonos players. No other target logs them; a global `RUST_LOG=debug` includes them. That traffic carries Handoffs' stream URLs, each stream's only key: in the SOAP requests that start a Handoff, and in the position answers Groups give while being listed and whenever a control of a Handoff reads where the Group is (`GetPositionInfo`: its `TrackURI`, possibly its metadata). Keep `sonos::raw` at DEBUG off where logs are shared. Nothing at `info` or above logs a stream URL or an API key.

### Sonos discovery

At startup and then every minute the Bridge searches for Sonos players with SSDP: an M-SEARCH to the multicast group `239.255.255.250:1900`, whose answers it collects for two seconds (players answer within one). A player that hasn't answered for three minutes is forgotten; at most 64 are known at once. An answer only counts if it is from a Sonos player (a `RINCON_` UUID of letters and digits) and its LOCATION is plain `http` to the IP address the answer came from (the device description itself is not fetched). The App's Groups list then asks the found players for the household's Group topology: first the one that told it last (if the Sonos app shows it), then the visible coordinators, the other visible players, players not in the last topology (which includes bonded home theatre surrounds and subs, listed there only as satellites), and last the hidden members (such as the second speaker of a stereo pair), since hidden and bonded speakers can tell an outdated topology. The next player is asked as soon as one fails, or after half a second if the one asked last hasn't answered. Of the topology, at most 64 Groups are kept, each coordinator once. Each Group's coordinator is asked what it plays at the address its own search answer gave, and the Group shows "Not answering" when no search found its coordinator, the coordinator failed to say, or it didn't say within the listing's 8 seconds (a state told in time is kept even if the track came too late; a track without a state isn't shown). If no player told the topology within those 8 seconds, the Groups list answers `sonos_unreachable`; if no player is known and the Bridge can't tell whether there are any (the last search could not be sent, searching stopped, or the first search didn't end in time), it answers `sonos_search_failed`. Listing Groups only reads (ZoneGroupTopology `GetZoneGroupState`, AVTransport `GetTransportInfo` and `GetPositionInfo`, GroupRenderingControl `GetGroupVolume`). The Bridge changes a Group's playback only when a household member starts a Handoff on it (AVTransport `SetAVTransportURI`, then `Play`, then `Seek` when it starts past the beginning, see [Play on a Group](#play-on-a-group)) or uses its controls (pause, resume, skips and volume, see [Control a Group](#control-a-group)); it never changes grouping.

`discovery_address` in `[sonos]` changes where the search goes. Leave it unset in production; the tests set it to a fake Sonos household (`crates/sonos-fake`) on loopback:

```toml
[sonos]
discovery_address = "127.0.0.1:41900"   # default "239.255.255.250:1900", SSDP multicast
```

The tests also set `clock_address` in `[testing]`, a test-only hook: the Bridge then times Handoff supervision (the Progress cadence and the end of an Item, see [Play on a Group](#play-on-a-group)) on the clock a fake Sonos household serves at that address, instead of the system clock, so a test moves it by hand. The Bridge logs a warning when it is set and won't start if it can't read that clock. Never set it in production.

### Encryption key

The Bridge encrypts the Audiobookshelf API keys it stores with a key you provide: 32 random bytes, base64-encoded, made with `openssl rand -base64 32`. It comes from the environment, never from the configuration file, which may sit on the same backed-up volume as the database:

1. `BRIDGE_ENCRYPTION_KEY_FILE`: the path of a file holding the key, such as a Docker secret. Whitespace around the key, like a trailing newline, is ignored; a file over 1024 bytes is refused.
2. `BRIDGE_ENCRYPTION_KEY`: the key itself. It stays in the Bridge's environment for as long as it runs, while the Bridge wipes the copies it makes while loading the key; prefer the file.

A variable set to the empty string counts as unset. When both are set, the file wins, even if it's missing or unusable (the Bridge never falls back to `BRIDGE_ENCRYPTION_KEY`); once the key has loaded, the Bridge logs one warning naming both variables. Without a usable key (neither variable set, an unreadable file, a blank value or file, not base64, or not 32 bytes) the Bridge exits with code 1 after one `cannot load the encryption key` error event, which names the variable and what is wrong, never the key, the file's contents or its path.

With Docker, the key is a Compose secret that only the container's user can read: see [Run with Docker](#run-with-docker).

Keep the key outside the database's backups, and keep it: with a different key the Bridge still starts, but can't decrypt the stored keys. It removes at once those that no session and no Handoff uses, and logs a warning if others remain (one for keys with sessions, one for keys Handoffs use). A session whose key it can't decrypt (another encryption key, or a tampered database) ends at its next call with `session_invalid`, and its stored key is removed, so the user logs in again. A Handoff whose key it can't decrypt ends the same way, when its stream is next asked for or the Handoffs are listed; a key only Handoffs use is kept until then. Switching back to the old key doesn't bring removed keys back.

## Run with Docker

The Bridge ships as the image `ghcr.io/stibbons-ai/clacks`, for `linux/amd64` and `linux/arm64`, built and smoke-tested by GitHub Actions ([image.yml](.github/workflows/image.yml)):

| Tag | Is |
|---|---|
| `X.Y.Z`, `X.Y.Z-rc.1` | the release tagged `vX.Y.Z`, the pre-release tagged `vX.Y.Z-rc.1` |
| `X.Y` | the most recently tagged release `vX.Y.*`, pre-releases excluded |
| `latest` | the most recently tagged release, pre-releases excluded |
| `main` | the newest published commit on `main` |
| `sha-<commit>` | a published commit on `main`, by its 7-character hash; a commit pushed while an earlier publish still runs may be skipped |

The image runs the Bridge as UID and GID 65532, with no shell. It reads the configuration from `/etc/clacks/bridge.toml` (set by `BRIDGE_CONFIG`) and keeps the database in `/var/lib/clacks`, which a fresh named volume takes over, owned by that user and readable only by it.

Next to `compose.yaml`, create the encryption key and make the container's user its owner, so that it alone can read the key, and write `bridge.toml` as in [Run the Bridge](#run-the-bridge), keeping that example's `listen` and `database`:

```sh
(umask 077; openssl rand -base64 32 > bridge_encryption_key)   # once; keep it, see "Encryption key"
sudo chown 65532:65532 bridge_encryption_key
```

```yaml
# compose.yaml
services:
  clacks:
    image: ghcr.io/stibbons-ai/clacks:main   # or a release tag, such as 0.1.0
    restart: unless-stopped
    init: true   # optional: the Bridge stops cleanly on `docker stop`'s SIGTERM without it too
    network_mode: host
    environment:
      BRIDGE_ENCRYPTION_KEY_FILE: /run/secrets/bridge_encryption_key
    secrets: [bridge_encryption_key]
    volumes:
      - ./bridge.toml:/etc/clacks/bridge.toml:ro
      - clacks-data:/var/lib/clacks
volumes:
  clacks-data:
secrets:
  bridge_encryption_key:
    file: ./bridge_encryption_key
```

`docker compose up -d` starts it; `docker compose logs clacks` shows a `listening` event once it serves. Instead of the secret, `BRIDGE_ENCRYPTION_KEY` in `environment` works too, but then anyone who can run `docker inspect` on the host sees the key. To upgrade, `docker compose pull && docker compose up -d`; the database stays in the volume. Back up the volume, without the key (see [Encryption key](#encryption-key)).

Each URL in `[[library_servers]]` must be reachable from the host; with host networking, an Audiobookshelf on the same host is reachable at `http://127.0.0.1:<its port>`.

### Why host networking

The container shares the host's network, so the Bridge listens on the host's addresses and needs no `ports:`. The Bridge finds Sonos players with SSDP multicast (see [Sonos discovery](#sonos-discovery)), and once playback control lands it also receives GENA event callbacks from the players on its own port ([ADR 0005](docs/adr/0005-local-upnp-sonos-control.md)); both need the host's LAN interface rather than a Docker bridge network. Keep the `listen` port fixed for the same reason.

Groups fetch what they play from the Bridge's own port too: each Handoff's stream, `http://<LAN address>:<listen port>/stream/<id>/<file>` ([ADR 0010](docs/adr/0010-media-streams-through-bridge.md)). The Bridge builds that URL from its own address, so speakers reach it on the host's LAN address directly, in plain HTTP, never through Traefik. With `listen` on every address (`0.0.0.0`), the Bridge gives each Group the host's address on the route to that Group's coordinator; with `listen` on one address, that one, which the speakers must then be able to reach. A firewall on the host must let the speakers in on the `listen` port.

### HTTPS through Traefik

The App can reach the Bridge over HTTPS at a stable hostname, such as `clacks.home.example.com`, with TLS terminated by Traefik. Plain HTTP on the LAN, such as `http://192.168.1.10:8080`, keeps working (the App's `Dioxus.toml` allows it), but sends the Audiobookshelf API key (at login) and the session token unencrypted. Traefik's Docker provider routes to a host-network container at `host.docker.internal` ([Traefik docs](https://doc.traefik.io/traefik/reference/install-configuration/providers/docker/#host-networking)), so give the Traefik container `extra_hosts: ["host.docker.internal:host-gateway"]`, and add to the `clacks` service these labels, in Traefik v3 syntax (v2 called the allow list `ipwhitelist`):

```yaml
    labels:
      traefik.enable: "true"
      traefik.http.routers.clacks.rule: Host(`clacks.home.example.com`)
      traefik.http.routers.clacks.entrypoints: websecure   # your HTTPS entry point
      traefik.http.routers.clacks.tls.certresolver: letsencrypt   # your certificate resolver
      traefik.http.routers.clacks.middlewares: clacks-private
      # Your LAN, which with the recommended subnet router on another LAN machine includes
      # tailnet devices (they arrive with that router's LAN address), and the tailnet's
      # addresses. Never add the Docker network's subnet.
      traefik.http.middlewares.clacks-private.ipallowlist.sourcerange: 192.168.1.0/24, 100.64.0.0/10
      traefik.http.services.clacks.loadbalancer.server.port: "8080"   # the `listen` port
```

As the hostname is never reachable from the internet, a Let's Encrypt certificate needs the [DNS challenge](https://doc.traefik.io/traefik/reference/install-configuration/tls/certificate-resolvers/acme/). The [IP allow list](https://doc.traefik.io/traefik/reference/routing-configuration/http/middlewares/ipallowlist/) is a second fence in case Traefik's entry point is ever exposed.

The Bridge's own port answers plain HTTP outside Traefik and its allow list: with host networking, `listen = "0.0.0.0:8080"` binds every interface of the host, including `tailscale0` when Tailscale runs on the host. Point the App only at the HTTPS hostname. To keep the plain port off other interfaces, set `listen` to the host's LAN address (say `"192.168.1.10:8080"`) and give Traefik that address instead: `extra_hosts: ["host.docker.internal:192.168.1.10"]`.

### Remote access through Tailscale only

Never forward a port to the Bridge or to its Traefik route: it has no public exposure. Away from home, the App reaches it through the household's [Tailscale](https://tailscale.com), in one of two ways, so the URL entered once works at home and away:

- **Recommended: a [subnet router](https://tailscale.com/kb/1019/subnets) on another LAN machine** advertises the LAN, and you approve the route in the admin console; iOS picks it up automatically. By default the router masquerades tailnet traffic, so it reaches Traefik from the router's LAN address, inside the allow list above. Tailnet devices reach the host's LAN address, so the hostname resolves to that same address everywhere: add the household's DNS server as a restricted nameserver for the hostname's domain ([split DNS](https://tailscale.com/kb/1054/dns)).
- **Tailscale on the Docker host** (advertising the LAN route or not): tailscaled masquerades the traffic it forwards into Docker, so Traefik sees the Docker network's gateway (`172.x`), which the allow list rejects with a 403. Turn that off with `sudo tailscale set --snat-subnet-routes=false` (Linux only; [subnet routers](https://tailscale.com/kb/1019/subnets), [CLI](https://tailscale.com/kb/1080/cli)), so Traefik sees the device's `100.x.y.z` address. If the host also advertises the LAN route, the other LAN devices then need a return route for `100.64.0.0/10` via the host. Without an advertised route, tailnet devices reach only the host's tailnet address, so the hostname must resolve to that address for them, through a restricted nameserver that answers with it, while the LAN's DNS keeps answering the LAN address.

After setup, make one request from a tailnet device and check the client address in Traefik's [access log](https://doc.traefik.io/traefik/reference/install-configuration/observability/logs-and-accesslogs/) (`ClientHost`): a 403 from the allow list means the source address isn't the one the list expects.

### Sonos: keep UPnP enabled

L-Space controls Sonos over its local UPnP interface ([ADR 0005](docs/adr/0005-local-upnp-sonos-control.md)); so far the Bridge reads which Groups there are, what they play and their volume (`GetZoneGroupState`, `GetTransportInfo`, `GetPositionInfo`, `GetGroupVolume`), starts a Handoff's stream on a Group (`SetAVTransportURI`, then `Play`, then `Seek` to the user's Progress when that is past the beginning, and `Pause` should that `Seek` fail), and controls a Group from the App: `Pause`, `Play` and `Seek` (`REL_TIME`) on AVTransport and `SnapshotGroupVolume`, then `SetGroupVolume`, on GroupRenderingControl, always on the Group's coordinator. It never changes grouping. Sonos lets you turn that interface off. Keep the UPnP setting in the Sonos app enabled: Account → Privacy & Security → Connection Security → UPnP. It is on by default ([Sonos: Adjust Connection Security settings](https://support.sonos.com/en-us/article/adjust-connection-security-settings)). Leave Authentication on the same screen at its default, off: turned on, it requires third-party integrations that use Sonos's LAN APIs to authenticate.

### Releases

Before each release, run the [release hardware checklist](docs/release-checklist.md) on real speakers and an iPhone.

## Log in

An App user logs in by picking a configured Library Server and entering an Audiobookshelf API key, which an Audiobookshelf admin creates for them (Settings → API Keys). The Bridge checks the key with Audiobookshelf and opens a session, which survives Bridge restarts and lasts until one of these:

- the user logs out;
- on a session call, Audiobookshelf rejects the key (HTTP 401: deleted, deactivated or expired key, or deactivated user), or the key now acts for another user; this ends this user's sessions opened with that key (a rejected login ends nothing);
- on a session call, the Bridge can't decrypt the stored key (it runs with another encryption key, see [Encryption key](#encryption-key)); this ends the sessions opened with that key, with `session_invalid`, and the Handoffs acting with it.

While the operator has removed its Library Server from the configuration, a session is unusable (`session_invalid`); re-adding the Library Server makes it usable again, and Audiobookshelf still checks the key on every call.

A user may log in with several keys, for example one per device; each session depends only on its own key. Sessions on different Library Servers are separate too: the App keeps one per Library Server and switches between them in Settings, and on Login once the selected session ended (see [Build and run the App](#build-and-run-the-app)); ending one leaves the others. While Audiobookshelf can't be reached (or answers anything but a usable success or 401, such as a 403 from a proxy in front of it), calls fail with `library_server_unreachable` and sessions stay.

## Play on a Group

On an Item's screen, "Play on…" lists every Group, and choosing one starts the Item there: a Handoff. It starts at the user's Progress in the Item (in whole seconds); a Finished Item, one without Progress, or one whose Progress, in whole seconds, lies at or past its end, starts from the beginning, and so does "Play from start…", next to "Play on…", which opens the same list. The Bridge stores the Handoff, gives the Group a stream URL on the Bridge (see [Why host networking](#why-host-networking)), tells it to play and then, starting past the beginning, to seek there; the Group then plays on its own, so the App can be closed, the phone leave the house, or its user log out. Any logged-in household member may start on any Group, and the App API lists every active Handoff to each of them. A Group has at most one: starting another on it replaces the old one, whose stream URL stops working. The old one is replaced before the Group is told to play, so if the Group then doesn't start or doesn't seek (any Sonos error: `group_failed`, `group_not_found` when it was regrouped meanwhile, `sonos_unreachable`, `sonos_search_failed`), it is left with no Handoff; a Group that played but didn't seek is also paused, so it doesn't play the Item from the beginning without one (the pause keeps the Group's turn up to 4 seconds; a start or control that can't wait that long answers `group_failed`). Starts on one Group take turns with each other and with controls on it. A start that had to wait for its turn, or whose Group had a start or control end while it resolved the Item, reads the user's Progress again once it has its turn (not for "Play from start…"), as what went before may have changed it (a pause writes it), and starts there; that read checks the key again like any session call (a rejected key is revoked, `api_key_rejected`; no answer within 2 seconds, `library_server_unreachable`), and if it fails nothing is stored or played. A start waits for its turn only as long as it can while still answering within about 14 seconds in all: resolving the Item, waiting for its turn, then, after a wait, up to 2 seconds to read Progress again, up to 4 for the Group to play and 4 more for it to seek (so it waits at most about 4 seconds, or 10 for "Play from start…", which reads nothing and doesn't seek); a start whose turn is free at once, with nothing ended on the Group while it resolved, needs only the 4 to play, and 4 to seek when it starts past the beginning. If its turn doesn't come in that time, or resolving the Item left too little of it for what follows, it answers `group_failed` and nothing is stored or played. Handoffs survive Bridge restarts.

The stream URL `/stream/<id>/<file>` needs no login, as speakers can't log in: its id, 256 random bits, is the only key, and works only for its Handoff's own audio file; any other id or file name gets `404`, alike. The Bridge forwards each request, with its byte range, to the Library Server with the API key of the user who started the Handoff, and answers with the audio and a `Content-Type` Sonos plays (`206` with the range, `200` without one, `416` for a range the file doesn't have). The key never leaves the Bridge: not in the URL, not in the answer. A Handoff ends when its starter's key is revoked (Audiobookshelf rejects it on a session call) or no longer decrypts (Audiobookshelf 2.37.2 may go on accepting a key an admin deleted or moved while a request with it was in flight, as it refills its key cache from the old row; that is outside the Bridge's control), and while the Library Server is down its stream answers `502`. The stream URL is checked on each request: once a Handoff ends (replaced, its key gone, or a start rolled back because the Group's `Play` or `Seek` failed or timed out after it had begun fetching), the next request gets `404`, but a request already being answered goes on to its end. Sonos may fetch a file in one long request or in ranges (for an index at the end of the file, and on seeks, see [the smoke test](docs/research/sonos-smoke-2026-11-04.md)), so a Group can keep playing what an open request still delivers until its next request; cutting open streams belongs to a later slice (supervision).

While a Group plays a Handoff, the Bridge writes the Group's position as Progress for the user who started it about every 30 seconds, as pausing does (see [Control a Group](#control-a-group)); nothing while the Group is paused or stopped, or plays something else. When the Group stops at the end of the Item, having played on to there, the Bridge writes Progress at the Item's end, Finished, at once, and the Handoff ends; nothing else starts on the Group. Pausing, even just before the end, never sets Finished, and every other Progress write during a Handoff leaves the Item unfinished, also one that was Finished and is played again. A start that replaces a Handoff first writes where the Group is on the replaced one (playing or paused there) as Progress for that Handoff's starter, with that Handoff's key, then has the Group play the new stream; that write gets only the time the start has left beyond the Group's calls (at most 4 seconds), and if it fails or runs out of time it is logged and the start goes on. Supervision happens in the Bridge's memory: after a restart, a surviving Handoff gets no regular Progress until it is started again. Each supervisor's reads and writes take turns with starts and controls on the Group, holding the turn at most about 2.5 seconds. How a real Sonos Group reports the end of a single track is unverified on hardware: the Bridge counts a Group as having played to the end when it finds it stopped on the Handoff's stream and, from where it was last seen playing and the time since, it should be within 5 seconds of the end; it doesn't rely on the position a stopped Group tells.

Not yet: only Items in a single audio file play (an episode, or an audiobook in one file), in a format Sonos plays by its extension (`item_not_playable` otherwise, such as for a `.opus` file; Opus inside an `.ogg` file passes that check and plays nothing, which a later repackaging slice handles); the Group is given no title or artwork; skips move by time within the one audio file, not by chapter; and a Takeover isn't detected: a Handoff whose Group plays something else stays active (the controls answer `no_handoff`) until it is replaced or its key revoked.

## Control a Group

The App's Groups screen offers, on each Group with an active Handoff, Pause or Resume (by what the Group is doing), "15 s back" and "30 s forward", and on each Group that told its volume, that volume with steps of 5 down and up. Any logged-in household member may use them on any Group, whoever started its Handoff; the volume works on a Group without a Handoff too. While one runs, the screen's controls are disabled.

The Bridge acts only while the Group plays its Handoff's stream: if the Group has no Handoff, or plays something else (what is a Takeover once detected, #20: changed in the Sonos app, say, so the Handoff isn't ended yet), the controls answer `no_handoff` (HTTP 409) and no control is sent to the Group (only the reads of what it plays). A Pause on a Group already paused, or a Resume on one already playing, sends nothing and writes nothing, and answers with the Group as it is. A skip goes from where the Group is, kept between the Item's start and its end (its length as the Group tells it, else as the Library Server does). When the Group tells no length, "30 s forward" first asks the Library Server, then reads again where the Group is and skips from there; if the Library Server doesn't tell the length (it is unreachable, too slow, or the Item is gone there), the skip is refused with `library_server_unreachable` and nothing is sent to the Group; if it rejects the starter's key, or the key now acts for another user, the key is revoked (see below) and the skip answers `no_handoff` (`internal` if storing the revoke failed); and if it answers but knows no length either, the skip is refused with `length_unknown` (HTTP 409) and nothing is sent to the Group or written. A Group that tells a length of zero counts as telling none. "15 s back" needs no length. A volume above 100 sets 100; the Bridge takes a fresh `SnapshotGroupVolume` right before each `SetGroupVolume`, so every member's volume changes in proportion to what it is now. Controls (and volume changes) on one Group take turns with each other and with starts on it; one that waited 3 seconds for its turn (the Group busy with another) answers `group_failed` without acting. A Group that is gone answers `group_not_found`; one that refuses, or whose answers to a control take more than 4 seconds in all, `group_failed`, and nothing is written then (except the read after a pause, which falls back as below). `group_failed` means the Bridge could not complete or confirm the control: a Group that answered too late may have acted all the same.

Pausing and skipping write the Group's new position at once as Progress in the Item for the user who started the Handoff, on their Library Server, with the API key the Handoff acts with: never the key of whoever pressed the button, and also after the starter logged out. A pause writes where the Group tells it paused, read right after the pause; if that read fails, is late, or tells no position of the Handoff's stream, where it was just before pausing. A Group that tells no position either time pauses, but no Progress is written. A skip writes its target. Before writing, the Bridge asks the Library Server whom the key acts for; if it is no longer the starter (an admin moved the key to another user), the key is revoked, which ends the Handoff, and nothing is written. Whether the starter may still see the Item is not asked again: Audiobookshelf doesn't check that on a Progress write, and the Bridge relies on the check made when the Handoff started. Resuming writes nothing. A write leaves the Item unfinished: the Bridge tells Audiobookshelf not to mark it Finished itself (which it otherwise does when less than 10 seconds of the Item's length remain), so "30 s forward" or a pause near the end doesn't; and a write that moves the position of a Finished Item makes it unfinished, so pausing or skipping an Item that was Finished and is played again (from the start) does. A control's requests to the Library Server (checking the key, looking up the length, writing) take at most 7 seconds in all; when only the write needs the length, its lookup gets at most half of what is left, and a late length is just left out of the write. With at most 3 seconds waiting for its turn and 4 for the Group, a control answers within about 14 seconds plus local storage, inside the App's 15; with less than a quarter of a second left, the write isn't sent. If the write fails or runs out of that time, the control still succeeds, as the Group did what was asked, and the Bridge logs a WARN ("Progress write not confirmed" when it ran out of time, as the write may have landed); if the Library Server rejected the starter's key, the key is revoked, which ends that user's sessions using it and the Handoff.
