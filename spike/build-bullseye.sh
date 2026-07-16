#!/usr/bin/env bash
# Spike: build spelunk release binaries inside a debian:11 (bullseye, glibc 2.31)
# container, mirroring the release.yml build step as closely as possible.
#
# Usage: build-bullseye.sh <amd64|arm64>
#   amd64 -> --platform linux/amd64 (Rosetta-emulated on Apple Silicon)
#   arm64 -> --platform linux/arm64 (native on Apple Silicon)
#
# Source is mounted read-only and copied into the container fs (virtiofs is
# slow for compile I/O; ro mount also proves the build does not rewrite
# Cargo.lock). Cargo home + target live in named volumes so reruns are warm.
set -euo pipefail

ARCH="${1:?usage: build-bullseye.sh <amd64|arm64>}"
case "$ARCH" in
  amd64) PLATFORM=linux/amd64; TARGET=x86_64-unknown-linux-gnu ;;
  arm64) PLATFORM=linux/arm64; TARGET=aarch64-unknown-linux-gnu ;;
  *) echo "unknown arch: $ARCH" >&2; exit 2 ;;
esac

SRC="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$SRC/spike/out-$ARCH"
mkdir -p "$OUT"

docker rm -f "${SPIKE_PREFIX:-spelunk-bullseye-spike}-build-$ARCH" 2>/dev/null || true

time docker run --name "${SPIKE_PREFIX:-spelunk-bullseye-spike}-build-$ARCH" --platform "$PLATFORM" \
  -v "$SRC:/src:ro" \
  -v "${SPIKE_PREFIX:-spelunk-bullseye-spike}-cargo-$ARCH:/cargo" \
  -v "${SPIKE_PREFIX:-spelunk-bullseye-spike}-target-$ARCH:/target" \
  -v "$OUT:/out" \
  -e CARGO_HOME=/cargo \
  -e RUSTUP_HOME=/cargo/rustup \
  -e CARGO_TARGET_DIR=/target \
  -e TARGET="$TARGET" \
  debian:11 bash -euc '
    export PATH=/cargo/bin:$PATH
    export DEBIAN_FRONTEND=noninteractive

    echo "=== container identity ==="
    uname -m
    cat /etc/debian_version
    ldd --version | head -1

    echo "=== apt deps ==="
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      curl ca-certificates build-essential pkg-config libdbus-1-dev
    dpkg -s libdbus-1-dev | grep -E "^(Package|Version):"
    gcc --version | head -1

    echo "=== rustup (needs glibc 2.17+; bullseye is 2.31) ==="
    if ! command -v rustc >/dev/null; then
      curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain stable
    fi
    rustc --version
    cargo --version

    echo "=== copy source into container fs ==="
    mkdir -p /build
    cp -a /src/. /build/
    rm -rf /build/.git /build/spike /build/tmp
    cd /build

    echo "=== build (mirrors release.yml) ==="
    # Local-only twist: the Docker VM here has ~7.6GB RAM; two concurrent
    # fat-LTO links (lto=true, codegen-units=1) OOM it. Compile everything in
    # parallel first (--lib), then run the exact CI command with -j1 so the
    # two bin links happen serially. GitHub runners (16GB) need none of this.
    date -u
    cargo build --release --target "$TARGET" --features rich-formats --lib
    cargo build --release --target "$TARGET" --features rich-formats -j1
    date -u

    echo "=== glibc ceiling (pre-strip) ==="
    for b in spelunk spelunk-server; do
      p="/target/$TARGET/release/$b"
      echo "-- $b max GLIBC_: $(objdump -T "$p" | grep -o "GLIBC_[0-9.]*" | sort -Vu | tail -1)"
      echo "-- $b NEEDED libs:"
      objdump -p "$p" | grep NEEDED
    done

    echo "=== strip + export ==="
    for b in spelunk spelunk-server; do
      strip "/target/$TARGET/release/$b"
      cp "/target/$TARGET/release/$b" /out/
    done
    ls -l /out
  '
rc=$?
docker rm -f "${SPIKE_PREFIX:-spelunk-bullseye-spike}-build-$ARCH" 2>/dev/null || true
exit $rc
