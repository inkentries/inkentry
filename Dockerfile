# spelunk-server — minimal local-scaffold image
#
# Multi-stage build: compile in a Rust builder, copy the binary into a slim
# Debian image. The result is a ~50 MB image with no Rust toolchain overhead.
#
# Build:
#   docker build -t spelunk-server .
#
# This image binds spelunk-server to 127.0.0.1 *inside its own container* by
# default (see CMD below) — that loopback lives in the container's private
# network namespace, so it is NOT reachable via `docker run -p ...` port
# publishing, Docker Desktop host-mode, or from a sibling container's DNS.
# That's intentional, not a bug: spelunk-server refuses to bind a non-loopback
# address over plaintext HTTP, unconditionally, keyed or not (see
# docs/server.md#non-loopback-plaintext-binds-are-refused-no-override), and
# this repo does not ship a proxy to pair with it.
#
# Run (dev, reachable only from other containers on the same Docker network):
#   docker network create spelunk-dev
#   docker run --rm --network spelunk-dev --name spelunk-server \
#     -v spelunk-data:/data spelunk-server
#   docker run --rm --network spelunk-dev curlimages/curl \
#     curl http://spelunk-server:7777/v1/health
#
# Run (local scaffold, with API key): see docker-compose.yml. It runs this
# image with a persistent volume — nothing more. It does not publish a
# host-reachable port; reach the server from a separate container sharing
# this one's network namespace (`--network container:spelunk-server`), the
# same pattern as the dev recipe above but pointed at 127.0.0.1.
#
# For a team-reachable deployment, don't containerize this at all: run the
# binary bare-metal/systemd on a host, with your own TLS terminator (nginx,
# Caddy, ...) in front of the same loopback bind on that host. See
# docs/self-hosting.md — that's the recommended path, since a container's
# loopback can't be handed to a same-host proxy the way a bare-metal
# process's can.

# ── Stage 1: build ────────────────────────────────────────────────────────────
FROM rust:1.96.1-slim AS builder

WORKDIR /build

# Cache dependency compilation separately from source changes.
COPY Cargo.toml Cargo.lock ./
COPY src/lib.rs src/lib.rs
# Placeholder main.rs so the dependency step compiles.
RUN mkdir -p src/bin && \
    echo 'fn main(){}' > src/main.rs && \
    echo 'fn main(){}' > src/bin/spelunk_server.rs && \
    cargo build --release --bin spelunk-server 2>/dev/null || true

# Now copy the real source and build properly.
COPY . .
RUN touch src/bin/spelunk_server.rs && \
    cargo build --release --bin spelunk-server

# ── Stage 2: runtime ──────────────────────────────────────────────────────────
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -r -s /bin/false spelunk
WORKDIR /data
RUN chown spelunk:spelunk /data

COPY --from=builder /build/target/release/spelunk-server /usr/local/bin/spelunk-server

USER spelunk

EXPOSE 7777

ENTRYPOINT ["/usr/local/bin/spelunk-server"]
# Bind loopback — the binary's own default, and the only bind this image
# supports. spelunk-server refuses to bind a non-loopback address over
# plaintext HTTP unconditionally, keyed or not (see
# docs/server.md#non-loopback-plaintext-binds-are-refused-no-override), so a
# `--host 0.0.0.0` override here would just make the server refuse to start —
# this image ships no proxy to pair with it. For a deployment that needs to be
# reachable off-host, don't override this; run bare-metal/systemd instead (see
# docs/self-hosting.md), where a same-host reverse proxy can front the
# server's loopback bind directly.
CMD ["--host", "127.0.0.1", "--db", "/data/spelunk.db"]
