# L-Space

Self-hosted playback of Audiobookshelf podcasts and audiobooks, on Sonos or on the phone. See [GLOSSARY.md](GLOSSARY.md) for the terms and [docs/adr](docs/adr) for the decisions.

## Build and test

Toolchain and tasks come from [mise](https://mise.jdx.dev). In a fresh clone run `mise trust` (mise ignores an untrusted `mise.toml`) and `mise install`, then

| Task | Does |
|---|---|
| `mise run build` | builds every crate and target |
| `mise run test` | runs every test, including the black-box Bridge tests and the App's flow tests |
| `mise run lint` | clippy with warnings denied, and `cargo fmt --check` |
| `mise run check` | all of the above |

The black-box Bridge tests start a real Audiobookshelf in a container ([testcontainers](https://crates.io/crates/testcontainers)), so `mise run test` needs a running Docker; the first run pulls the Audiobookshelf image. The App's flow tests (`crates/app/tests`) drive the App's state and actions, without rendering, against a real Bridge and Audiobookshelf; they start the `bridge` binary from the same target directory, so run them through `mise run test` (or `cargo build -p bridge` before `cargo test -p app`). They run apart from the rest of the workspace: built together with the Bridge, `api` gets its `server` feature, and the App's server function calls would run in-process instead of over HTTP.

Crates live in `crates/`: `api` (the App API contract), `bridge` (the Bridge binary), `app` (the App), `abs` (the Audiobookshelf client), `store` (the storage traits and their contract test suite), `store-sqlite` (their SQLite implementation) and `test-support` (the test harness the Bridge's and the App's tests share).

## Build and run the App

The App (shipped as Ook; its screens call the Bridge Clacks) is a [Dioxus](https://dioxuslabs.com) 0.8 mobile app (ADR 0007), built with Dioxus's CLI `dx`, which `crates/app/mise.toml` pins to the workspace's `dioxus` version (0.8.0-alpha.3). It also points `DEVELOPER_DIR` at `/Applications/Xcode.app`, so iOS builds use Xcode without `xcode-select -s`. Run `dx` from `crates/app` through mise, which also shadows Homebrew's unrelated `dx` (Deno):

```sh
cd crates/app
mise trust && mise install                  # once: Rust's iOS targets and dx
mise exec -- dx serve --ios                 # iOS Simulator: builds, installs and launches on the booted simulator
mise exec -- dx serve --ios --release --device "<iPhone name>" \
  --codesign --apple-team-id "Apple Development: <Name> (<TEAMID>)"   # a phone; device setup as in spikes/ios-audio/README.md
```

The bundle identifier is `ai.stibbons.ook`. On first launch the App asks for the Bridge's address, for example `http://192.168.1.10:8080` (the Simulator reaches a Bridge on the same Mac at `http://127.0.0.1:8080`); the Bridge answers with its Library Servers, and the App remembers the address. Then a person picks a Library Server and enters their Audiobookshelf API key (see [Log in](#log-in)). The App keeps the address, the session and the language choice in `Application Support/ai.stibbons.ook/app.json` inside its sandbox, readable by its owner only; the API key is not kept. The session lasts across launches until logout, or until the Bridge reports the key revoked or the session ended; if the Bridge can't be reached at launch the App stays logged in. Logout also forgets the session when the Bridge can't be reached.

Every string comes from Fluent (`crates/app/locales/en.ftl`, `de.ftl`, embedded in the binary). The App shows German on a German device and English otherwise, unless Settings picks a language.

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

A user may log in with several keys, for example one per device; each session depends only on its own key. While Audiobookshelf can't be reached (or answers anything but a usable success or 401, such as a 403 from a proxy in front of it), calls fail with `library_server_unreachable` and sessions stay.
