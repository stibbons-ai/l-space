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
cargo run -p bridge -- --config bridge.toml   # or BRIDGE_CONFIG=bridge.toml
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

The database holds the Bridge's state: App sessions and the Audiobookshelf API keys users logged in with, in plain text for now, so on Unix the Bridge makes it readable by its owner only, also when it already existed. Logout ends only the session: the key stays stored, and so do keys and sessions of Library Servers removed from the configuration. A database written by a newer Bridge is refused.

Logs are JSON on stdout, one event per line. `RUST_LOG` adds to the default level `info`: `RUST_LOG=hyper=debug` raises one target and leaves the rest at `info`, and `RUST_LOG=warn` lowers the default.

## Log in

An App user logs in by picking a configured Library Server and entering an Audiobookshelf API key, which an Audiobookshelf admin creates for them (Settings → API Keys). The Bridge checks the key with Audiobookshelf and opens a session, which survives Bridge restarts and lasts until one of these:

- the user logs out;
- on a session call, Audiobookshelf rejects the key (HTTP 401: deleted, deactivated or expired key, or deactivated user), or the key now acts for another user; this ends this user's sessions opened with that key (a rejected login ends nothing).

While the operator has removed its Library Server from the configuration, a session is unusable (`session_invalid`); re-adding the Library Server makes it usable again, and Audiobookshelf still checks the key on every call.

A user may log in with several keys, for example one per device; each session depends only on its own key. While Audiobookshelf can't be reached (or answers anything but success or 401, such as a 403 from a proxy in front of it), calls fail with `library_server_unreachable` and sessions stay.
