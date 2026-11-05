# syntax=docker/dockerfile:1

# The Bridge (Clacks) as a container image. Built natively per platform (linux/amd64,
# linux/arm64); see .github/workflows/image.yml and the README's "Run with Docker".

# Keep in step with the toolchain in mise.toml.
ARG RUST_VERSION=1.99.1

FROM rust:${RUST_VERSION}-slim-trixie AS build
# The data directory, created here because the runtime image has no shell. A fresh named volume
# mounted over it takes its owner and mode, so the Bridge can create its database there, and
# only the Bridge's user can list it.
RUN mkdir -p /var/lib/clacks
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --package bridge \
    && cp target/release/bridge /usr/local/bin/bridge

# glibc, libgcc and CA roots, no shell; runs as `nonroot` (UID and GID 65532).
FROM gcr.io/distroless/cc-debian13:nonroot
LABEL org.opencontainers.image.title="Clacks" \
      org.opencontainers.image.description="The L-Space Bridge: Audiobookshelf playback on Sonos or on the phone" \
      org.opencontainers.image.source="https://github.com/stibbons-ai/l-space" \
      org.opencontainers.image.licenses="MIT"
COPY --from=build /usr/local/bin/bridge /usr/local/bin/bridge
COPY --from=build --chown=65532:65532 --chmod=0700 /var/lib/clacks /var/lib/clacks
USER 65532:65532
ENV BRIDGE_CONFIG=/etc/clacks/bridge.toml
ENTRYPOINT ["/usr/local/bin/bridge"]
