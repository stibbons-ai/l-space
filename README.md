# L-Space

Self-hosted playback of Audiobookshelf podcasts and audiobooks, on Sonos or on the phone. See [GLOSSARY.md](GLOSSARY.md) for the terms and [docs/adr](docs/adr) for the decisions.

## Build and test

Toolchain and tasks come from [mise](https://mise.jdx.dev). In a fresh clone run `mise trust` (mise ignores an untrusted `mise.toml`) and `mise install`, then

| Task | Does |
|---|---|
| `mise run build` | builds every crate and target |
| `mise run test` | runs every test, including the black-box Bridge tests |
| `mise run lint` | clippy with warnings denied, and `cargo fmt --check` |
| `mise run check` | all of the above |

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

The database holds the Bridge's state: App sessions and the Audiobookshelf API keys users logged in with. Each key is encrypted (XChaCha20-Poly1305, bound to its Library Server and user) and is looked up by an HMAC keyed by the encryption key, never by a plain hash; session tokens are stored only as their SHA-256. On Unix the Bridge also makes the database readable by its owner only, also when it already existed. Logout ends only the session: the key stays stored, and so do keys and sessions of Library Servers removed from the configuration. A database written by a newer Bridge is refused. Upgrading a database from a Bridge that stored keys in plain text deletes those keys and their sessions, so their users log in again; the Bridge then rebuilds the file (`VACUUM`) so that neither those keys nor keys the earlier Bridge had deleted remain in it. During the upgrade itself SQLite's rollback journal briefly holds copies of the old pages; it is deleted, not overwritten, afterwards.

Logs are JSON on stdout, one event per line. `RUST_LOG` adds to the default level `info`: `RUST_LOG=hyper=debug` raises one target and leaves the rest at `info`, and `RUST_LOG=warn` lowers the default.

### Encryption key

The Bridge encrypts the Audiobookshelf API keys it stores with a key you provide: 32 random bytes, base64-encoded, made with `openssl rand -base64 32`. It comes from the environment, never from the configuration file, which may sit on the same backed-up volume as the database:

1. `BRIDGE_ENCRYPTION_KEY_FILE`: the path of a file holding the key, such as a Docker secret. Whitespace around the key, like a trailing newline, is ignored; a file over 1024 bytes is refused.
2. `BRIDGE_ENCRYPTION_KEY`: the key itself.

A variable set to the empty string counts as unset. When both are set, the file wins, even if it's missing or unusable (the Bridge never falls back to `BRIDGE_ENCRYPTION_KEY`); once the key has loaded, the Bridge logs one warning naming both variables. Without a usable key (neither variable set, an unreadable file, a blank value or file, not base64, or not 32 bytes) the Bridge exits with code 1 after one `cannot load the encryption key` error event, which names the variable and what is wrong, never the key, the file's contents or its path.

```yaml
# compose.yaml (excerpt)
services:
  clacks:
    environment:
      BRIDGE_ENCRYPTION_KEY_FILE: /run/secrets/bridge_encryption_key
    secrets: [bridge_encryption_key]
secrets:
  bridge_encryption_key:
    file: ./bridge_encryption_key   # (umask 077; openssl rand -base64 32 > bridge_encryption_key)
```

The key file must be readable by the user the container runs as, and by no one else.

Keep the key outside the database's backups, and keep it: with a different key the Bridge still starts, but can't decrypt the stored keys. It removes at once those that no session uses, and logs a warning if others remain. A session whose key it can't decrypt (another encryption key, or a tampered database) ends at its next call with `session_invalid`, and its stored key is removed, so the user logs in again. Switching back to the old key doesn't bring removed keys back.

## Log in

An App user logs in by picking a configured Library Server and entering an Audiobookshelf API key, which an Audiobookshelf admin creates for them (Settings → API Keys). The Bridge checks the key with Audiobookshelf and opens a session, which survives Bridge restarts and lasts until one of these:

- the user logs out;
- on a session call, Audiobookshelf rejects the key (HTTP 401: deleted, deactivated or expired key, or deactivated user), or the key now acts for another user; this ends this user's sessions opened with that key (a rejected login ends nothing);
- on a session call, the Bridge can't decrypt the stored key (it runs with another encryption key, see [Encryption key](#encryption-key)); this ends the sessions opened with that key, with `session_invalid`.

While the operator has removed its Library Server from the configuration, a session is unusable (`session_invalid`); re-adding the Library Server makes it usable again, and Audiobookshelf still checks the key on every call.

A user may log in with several keys, for example one per device; each session depends only on its own key. While Audiobookshelf can't be reached (or answers anything but a usable success or 401, such as a 403 from a proxy in front of it), calls fail with `library_server_unreachable` and sessions stay.
