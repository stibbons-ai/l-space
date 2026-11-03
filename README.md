# L-Space

Self-hosted playback of Audiobookshelf podcasts and audiobooks, on Sonos or on the phone. See [GLOSSARY.md](GLOSSARY.md) for the terms and [docs/adr](docs/adr) for the decisions.

## Build and test

Toolchain and tasks come from [mise](https://mise.jdx.dev): `mise install`, then

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
listen = "0.0.0.0:8080"

[[library_servers]]
url = "https://abs.example.com"   # Audiobookshelf base URL, may include a subpath
name = "Home"                     # optional
```

Each Library Server is identified by its normalized base URL: lowercase scheme and host, no default port, no trailing slash. Only `http` and `https` are accepted; a query, fragment, user name or password is rejected, as are two entries that normalize to the same URL. An invalid configuration stops the Bridge with exit code 1 and one `invalid configuration` error event naming the entry.

Logs are JSON on stdout, one event per line. `RUST_LOG` sets the level (default `info`).
