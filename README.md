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

Crates live in `crates/`: `api` (the App API contract) and `bridge` (the Bridge binary).

## Run the Bridge

```sh
cargo run -p bridge -- --config bridge.toml   # or BRIDGE_CONFIG=bridge.toml
```

```toml
# bridge.toml
listen = "0.0.0.0:8080"           # required; port 0 picks a free port

[[library_servers]]
url = "https://abs.example.com"   # Audiobookshelf base URL, may include a subpath
name = "Home"                     # optional
```

Each Library Server is identified by its normalized base URL: lowercase scheme and host, no default port, no trailing slash. Only `http` and `https` are accepted; a query, fragment, user name or password is rejected, as are two entries that normalize to the same URL. Any `@` counts as a user name or password and is rejected, so a password with an unencoded `/`, `?` or `#` can't pass for a host or path.

A configuration that can't be read or parsed, or a bad Library Server URL, stops the Bridge with exit code 1 and one `invalid configuration` error event. For a bad URL the event names the entry and shows only the URL's scheme, host, port and path (a query or fragment as `REDACTED`), or nothing of it when it contains `@`, can't be parsed or has no host; for a TOML error it gives the line and column where known, never the source line. If the `listen` address can't be bound, the Bridge exits with code 1 after one `cannot listen` error event.

Logs are JSON on stdout, one event per line. `RUST_LOG` adds to the default level `info`: `RUST_LOG=hyper=debug` raises one target and leaves the rest at `info`, and `RUST_LOG=warn` lowers the default.
