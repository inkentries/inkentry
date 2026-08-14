# inkentry-server — container image for a local scaffold or a team server
#
# Multi-stage build: compile in a Rust builder, copy the binary into a slim
# Debian image. The result is a ~50 MB image with no Rust toolchain overhead.
#
# Build:
#   docker build -t inkentry-server .
#
# This image binds inkentry-server to 127.0.0.1 *inside its own container* by
# default (see CMD below) — that loopback lives in the container's private
# network namespace, so it is NOT reachable via `docker run -p ...` port
# publishing, Docker Desktop host-mode, or from a sibling container's DNS.
# That default is the local-scaffold shape, not a ceiling. What the server
# refuses unconditionally is a non-loopback bind over *plaintext* HTTP, keyed
# or not (see
# docs/server-setup.md#non-loopback-plaintext-binds-are-refused-no-override);
# a routable bind carrying TLS and an API key is supported and is how the
# team-server deployment runs.
#
# Run (dev, no compose): the server binds loopback only (see above), so a
# sibling container on its own network can't reach it: sibling-container DNS
# resolves to the bridge IP, and nothing listens there. A sidecar has to share
# the server's network namespace instead, then reach it at 127.0.0.1:
#   docker run -d --name inkentry-server -v inkentry-data:/data inkentry-server
#   docker run --rm --network container:inkentry-server curlimages/curl \
#     curl http://127.0.0.1:7777/v1/health
#
# Run (local scaffold, with API key): see docker-compose.yml. It runs this
# image with a persistent volume, wired up with the same
# `--network container:inkentry-server` + 127.0.0.1 pattern above. Nothing
# more; it does not publish a host-reachable port.
#
# Run (team-reachable): override the bind and let the server terminate HTTPS
# itself (ADR-066) — `--host 0.0.0.0` with `--tls-cert`, `--tls-key` and an API
# key, nothing in front of it. docker-compose.yml's `team-server` profile
# builds this image and wires exactly that up; see
# docs/server-setup.md#4-docker-a-team-server-or-a-local-scaffold. The
# bare-metal/systemd path (docs/server-setup.md#3-run-it-under-systemd) is the
# same shape without a container.

# ── Stage 1: build ────────────────────────────────────────────────────────────
FROM rust:1.97.1-slim AS builder

WORKDIR /build

# System build deps the slim image lacks: a C/C++ toolchain for tokenizers'
# esaxx-rs build script (embed-native default), and libdbus-1-dev to satisfy
# libdbus-sys's build script (pulled via keyring's sync-secret-service backend).
# Build-time only — the linker strips the unused lib, so the runtime image
# needs no dbus package.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config libdbus-1-dev \
    && rm -rf /var/lib/apt/lists/*

# Cache dependency compilation separately from source changes. This is a
# virtual Cargo workspace (no root package), so prime the cache from the
# workspace manifests plus a placeholder source per member crate; the heavy
# third-party deps (candle, etc.) then land in a layer that only busts when a
# Cargo.toml / Cargo.lock changes. Every member manifest must be present and
# its declared target source must exist, or cargo refuses to load the
# workspace — even members the server bin doesn't depend on, and even targets
# nothing here builds: an explicit `[[example]]`/`[[bin]]` block is a declared
# target, so adding one means adding a placeholder for it below.
COPY Cargo.toml Cargo.lock ./
COPY crates/inkentry-core/Cargo.toml   crates/inkentry-core/Cargo.toml
COPY crates/inkentry-cli/Cargo.toml    crates/inkentry-cli/Cargo.toml
COPY crates/inkentry-embed/Cargo.toml  crates/inkentry-embed/Cargo.toml
COPY crates/inkentry-server/Cargo.toml crates/inkentry-server/Cargo.toml
RUN mkdir -p crates/inkentry-core/src crates/inkentry-cli/src \
             crates/inkentry-embed/src crates/inkentry-server/src \
             crates/inkentry-core/examples && \
    : > crates/inkentry-core/src/lib.rs && \
    : > crates/inkentry-embed/src/lib.rs && \
    : > crates/inkentry-server/src/lib.rs && \
    echo 'fn main(){}' > crates/inkentry-cli/src/main.rs && \
    echo 'fn main(){}' > crates/inkentry-server/src/main.rs && \
    echo 'fn main(){}' > crates/inkentry-core/examples/chunk_quality_eval.rs && \
    cargo build --release --bin inkentry-server && \
    rm -rf crates/*/src crates/inkentry-core/examples

# Now copy the real source and build properly. BuildKit normalizes COPY mtimes
# to a constant OLDER than the cached placeholder artifacts, so cargo's
# freshness check would reuse the placeholder binary. `touch` every crate
# source so the real build supersedes the cache.
COPY . .
RUN find crates -name '*.rs' -exec touch {} + && \
    cargo build --release --bin inkentry-server

# ── Stage 2: runtime ──────────────────────────────────────────────────────────
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# `-m -d /data` gives the service user a real home at the same path the
# volume is mounted, so any `$HOME`-relative path a dependency resolves stays
# writable; `useradd -m` also creates /data pre-owned by inkentry, so no
# separate chown is needed. WORKDIR after useradd picks up that existing dir.
RUN useradd -r -m -d /data -s /bin/false inkentry
WORKDIR /data

COPY --from=builder /build/target/release/inkentry-server /usr/local/bin/inkentry-server

# Primary fix: point the embedder's model cache at the persistent /data
# volume instead of the default $HOME/.local/share resolution. Without this,
# a fresh container re-downloads the ~339 MB model into the container layer
# on every `docker rm`/recreate even once $HOME itself is writable (see
# useradd above).
ENV XDG_DATA_HOME=/data

USER inkentry

EXPOSE 7777

ENTRYPOINT ["/usr/local/bin/inkentry-server"]
# Bind loopback — the binary's own default, and the right default here: this
# bind serves plain HTTP with no API key required, which is only safe while
# nothing off-host can reach it. Override it for a team-reachable deployment,
# but a non-loopback bind must carry both TLS (`--tls-cert`/`--tls-key`) and an
# API key or the server refuses to start (see
# docs/server-setup.md#non-loopback-plaintext-binds-are-refused-no-override).
# docker-compose.yml's `team-server` profile is that override, done for you.
CMD ["--host", "127.0.0.1", "--db", "/data/inkentry.db"]
