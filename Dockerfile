# spelunk-server — minimal production image
#
# Multi-stage build: compile in a Rust builder, copy the binary into a slim
# Debian image. The result is a ~50 MB image with no Rust toolchain overhead.
#
# Build:
#   docker build -t spelunk-server .
#
# Run (dev, no auth, reachable only from other containers on the same Docker
# network — a bare `docker run` cannot publish a container-loopback bind to
# the host; see docker-compose.yml for the supported "reach it from the host /
# off-host" shape):
#   docker network create spelunk-dev
#   docker run --rm --network spelunk-dev --name spelunk-server \
#     -v spelunk-data:/data spelunk-server
#   docker run --rm --network spelunk-dev curlimages/curl \
#     curl http://spelunk-server:7777/v1/health
#
# Run (production, with API key): see docker-compose.yml. A keyed deployment
# meant to be reached off-host MUST put a TLS-terminating reverse proxy in
# front — spelunk-server refuses to bind non-loopback over plaintext HTTP,
# unconditionally, keyed or not (see docs/server.md#trust-model). The compose
# file wires this up as a Caddy sidecar sharing this container's network
# namespace (loopback inside the container IS spelunk-server's loopback from
# there), so this image's own default stays loopback-only and a bare
# `docker run -p 7777:7777 ...` of this image will NOT expose it — that's
# intentional, not a bug; use docker-compose.yml to publish a port.

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
# Bind loopback (the binary's own default) and let a reverse proxy — not this
# container — be the thing that's reachable off-host. spelunk-server refuses
# to bind a non-loopback address over plaintext HTTP unconditionally, keyed or
# not (see docs/server.md#trust-model), so a bare `--host 0.0.0.0` here would
# make a keyed deployment (docker-compose.yml) refuse to start. To publish the
# server directly on the container's own interface anyway (e.g. behind your
# own external TLS termination that isn't part of this compose file, or a
# trusted-network dev setup with no key), override the CMD with
# `--host 0.0.0.0` explicitly — this is the same non-loopback bind the server
# itself will refuse unless it's paired with a proxy in front.
CMD ["--host", "127.0.0.1", "--db", "/data/spelunk.db"]
