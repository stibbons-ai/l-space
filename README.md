# L-Space

Self-hosted playback of Audiobookshelf podcasts and audiobooks, on Sonos or on the phone. See [GLOSSARY.md](GLOSSARY.md) for the terms and [docs/adr](docs/adr) for the decisions.

## Build and test

Toolchain and tasks come from [mise](https://mise.jdx.dev). In a fresh clone run `mise trust` (mise ignores an untrusted `mise.toml`) and `mise install`, then

| Task | Does |
|---|---|
| `mise run build` | builds every crate and target |
| `mise run test` | runs every test, including the black-box Bridge tests |
| `mise run lint` | clippy with warnings denied, and `cargo fmt --check` |
| `mise run check` | build, lint and test |
| `mise run image` | builds the Clacks image for this machine's platform and smoke-tests it ([scripts/smoke-image.sh](scripts/smoke-image.sh)) |

The black-box Bridge tests start a real Audiobookshelf in a container ([testcontainers](https://crates.io/crates/testcontainers)), so `mise run test` needs a running Docker; the first run pulls the Audiobookshelf image.

Crates live in `crates/`: `api` (the App API contract), `bridge` (the Bridge binary), `abs` (the Audiobookshelf client), `store` (the storage traits and their contract test suite) and `store-sqlite` (their SQLite implementation).

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

The database holds the Bridge's state: App sessions and the Audiobookshelf API keys users logged in with. Each key is encrypted (XChaCha20-Poly1305, bound to its Library Server and user) and is looked up by an HMAC keyed by the encryption key, never by a plain hash; session tokens are stored only as their SHA-256. On Unix the Bridge also makes the database readable by its owner only, also when it already existed. Logout ends only the session: the key stays stored, and so do keys and sessions of Library Servers removed from the configuration. A database written by a newer Bridge is refused. Upgrading a database from a Bridge that stored keys in plain text deletes those keys and their sessions, so their users log in again. Before that, the Bridge rebuilds the file (`VACUUM`), so that keys the earlier Bridge had deleted don't linger in free pages; if the rebuild fails, nothing is upgraded and the next start tries again. The upgrade's own deletes are overwritten in the file. During the upgrade SQLite's rollback journal briefly holds copies of the old pages; it is deleted, not overwritten, afterwards, and a crash mid-upgrade leaves it in place until the next start rolls it back.

Logs are JSON on stdout, one event per line. `RUST_LOG` adds to the default level `info`: `RUST_LOG=hyper=debug` raises one target and leaves the rest at `info`, and `RUST_LOG=warn` lowers the default.

### Encryption key

The Bridge encrypts the Audiobookshelf API keys it stores with a key you provide: 32 random bytes, base64-encoded, made with `openssl rand -base64 32`. It comes from the environment, never from the configuration file, which may sit on the same backed-up volume as the database:

1. `BRIDGE_ENCRYPTION_KEY_FILE`: the path of a file holding the key, such as a Docker secret. Whitespace around the key, like a trailing newline, is ignored; a file over 1024 bytes is refused.
2. `BRIDGE_ENCRYPTION_KEY`: the key itself.

A variable set to the empty string counts as unset. When both are set, the file wins, even if it's missing or unusable (the Bridge never falls back to `BRIDGE_ENCRYPTION_KEY`); once the key has loaded, the Bridge logs one warning naming both variables. Without a usable key (neither variable set, an unreadable file, a blank value or file, not base64, or not 32 bytes) the Bridge exits with code 1 after one `cannot load the encryption key` error event, which names the variable and what is wrong, never the key, the file's contents or its path.

With Docker, the key is a Compose secret that only the container's user can read: see [Run with Docker](#run-with-docker).

Keep the key outside the database's backups, and keep it: with a different key the Bridge still starts, but can't decrypt the stored keys. It removes at once those that no session uses, and logs a warning if others remain. A session whose key it can't decrypt (another encryption key, or a tampered database) ends at its next call with `session_invalid`, and its stored key is removed, so the user logs in again. Switching back to the old key doesn't bring removed keys back.

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
    init: true   # passes `docker stop`'s SIGTERM on; the Bridge has no signal handling yet, so without it, stopping waits 10 s and kills it
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

The container shares the host's network, so the Bridge listens on the host's addresses and needs no `ports:`. Sonos control isn't implemented yet; once it is, the Bridge finds Groups with SSDP multicast and receives GENA event callbacks from the speakers on its own port ([ADR 0005](docs/adr/0005-local-upnp-sonos-control.md)), which needs the host's LAN interface rather than a Docker bridge network. Keep the `listen` port fixed for the same reason.

### HTTPS through Traefik

Once there is an App, it reaches the Bridge over HTTPS at a stable hostname, such as `clacks.home.example.com`, with TLS terminated by Traefik. Traefik's Docker provider routes to a host-network container at `host.docker.internal` ([Traefik docs](https://doc.traefik.io/traefik/reference/install-configuration/providers/docker/#host-networking)), so give the Traefik container `extra_hosts: ["host.docker.internal:host-gateway"]`, and add to the `clacks` service these labels, in Traefik v3 syntax (v2 called the allow list `ipwhitelist`):

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

L-Space will control Sonos over its local UPnP interface once Sonos control lands ([ADR 0005](docs/adr/0005-local-upnp-sonos-control.md)), and Sonos lets you turn that interface off. Keep the UPnP setting in the Sonos app enabled: Account → Privacy & Security → Connection Security → UPnP. It is on by default ([Sonos: Adjust Connection Security settings](https://support.sonos.com/en-us/article/adjust-connection-security-settings)). Leave Authentication on the same screen at its default, off: turned on, it requires third-party integrations that use Sonos's LAN APIs to authenticate.

### Releases

Before each release, run the [release hardware checklist](docs/release-checklist.md) on real speakers and an iPhone.

## Log in

An App user logs in by picking a configured Library Server and entering an Audiobookshelf API key, which an Audiobookshelf admin creates for them (Settings → API Keys). The Bridge checks the key with Audiobookshelf and opens a session, which survives Bridge restarts and lasts until one of these:

- the user logs out;
- on a session call, Audiobookshelf rejects the key (HTTP 401: deleted, deactivated or expired key, or deactivated user), or the key now acts for another user; this ends this user's sessions opened with that key (a rejected login ends nothing);
- on a session call, the Bridge can't decrypt the stored key (it runs with another encryption key, see [Encryption key](#encryption-key)); this ends the sessions opened with that key, with `session_invalid`.

While the operator has removed its Library Server from the configuration, a session is unusable (`session_invalid`); re-adding the Library Server makes it usable again, and Audiobookshelf still checks the key on every call.

A user may log in with several keys, for example one per device; each session depends only on its own key. While Audiobookshelf can't be reached (or answers anything but a usable success or 401, such as a 403 from a proxy in front of it), calls fail with `library_server_unreachable` and sessions stay.
