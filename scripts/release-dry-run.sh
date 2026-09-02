#!/usr/bin/env bash
#
# scripts/release-dry-run.sh
#
# Local, docker-based dry run of the Linux leg of .github/workflows/release.yml:
# build inside debian:11 (the glibc 2.31 floor), enforce the glibc ceiling on
# the resulting binaries, assemble the .deb with its Depends line derived by
# dpkg-shlibdeps run inside debian:11, then install and smoke-test the .deb in
# a fresh floor container. Run this before pushing a version tag, to catch
# release breakage on a dev machine instead of at tag-push time.
#
# What this proves: the Linux x86_64 build links against the glibc floor,
# the .deb's Depends line is floor-derived (so it can actually install on
# debian:11 / ubuntu:20.04), and the installed package survives real
# subcommands, not just `apt-get install`.
#
# What this does NOT prove: macOS or Windows builds, the arm64 Linux leg,
# the actual GitHub Release, or the Homebrew/Scoop publish steps. Those are
# only exercised by release.yml at real tag-push time. This script has no
# code path that can create a GitHub release, push to homebrew-inkentry, or
# write bucket/inkentry.json -- see the "explicitly does not touch" note
# below the stage functions.
#
# Usage:
#   scripts/release-dry-run.sh
#
# Requires: Docker only. No GITHUB_TOKEN, no tag push, no write access to
# any repo other than this script's own gitignored output directory
# (target/release-dry-run/).
#
# Env overrides (optional):
#   SMOKE_IMAGES  space-separated list of images to install+smoke-test the
#                 built .deb in. Defaults to the three floor/current images
#                 release.yml's own "deb" job smoke-tests against.

# Every single-quoted block below is a container-side script, expanded by bash
# inside the container rather than by this shell. shellcheck understands that
# shape only when `docker run` is the literal command word, so routing the
# containers through the run_docker wrapper makes it flag all of them.
# shellcheck disable=SC2016

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

TARGET="x86_64-unknown-linux-gnu"
FEATURES="rich-formats,llama-vulkan"
# Pinned Vulkan SDK (build-time only: glslc + headers/loader for the
# llama-vulkan feature). Mirrors the release.yml pin; bump both together.
VULKAN_SDK_VERSION="1.4.357.1"
VULKAN_SDK_SHA256="4b41e3b30e8aedaa5dac7c136561ab463eb316a25a54e2c6245f2c299ea1fb85"
BUILD_IMAGE="debian:11"
SMOKE_IMAGES="${SMOKE_IMAGES:-debian:11 ubuntu:20.04 ubuntu:24.04}"
# Every container below must run as amd64, matching the amd64 target/.deb --
# on an arm64 host (e.g. Apple Silicon), Docker silently resolves some image
# tags to a native arm64 manifest and others to amd64 depending on what's
# cached, with no warning either way. Without pinning this, a floor-image
# smoke test can fail with "Depends: libc6:amd64 ... not installable" that
# has nothing to do with the .deb's real installability -- the container
# itself has no amd64 architecture enabled. Pinning makes every stage
# deterministic regardless of host architecture.
DOCKER_PLATFORM="linux/amd64"

WORKDIR="target/release-dry-run"
CACHE_CARGO="${WORKDIR}/cache/cargo"
CACHE_RUSTUP="${WORKDIR}/cache/rustup"
LOCK_DIR="${WORKDIR}/.lock"

VERSION="$(grep -m1 '^version' crates/inkentry-cli/Cargo.toml | sed -E 's/version *= *"([^"]+)"/\1/')-dryrun"
DEB_VERSION="${VERSION}"

# Per-invocation, so a second run cannot overwrite the layout and .deb a first
# run is still reading. The cache dirs above stay shared on purpose -- scoping
# them per run would turn every invocation into a 15-20 minute cold build.
RUN_DIR="${WORKDIR}/run-$$"
DEB_LAYOUT="${RUN_DIR}/BUILD"
DEB_PATH="${RUN_DIR}/inkentry_${DEB_VERSION}_amd64.deb"

CONTAINER_PREFIX="inkentry-release-dry-run-$$"
STAGE="init"
# Name of the container currently running, or empty between stages. The signal
# trap reads this to kill the in-progress container instead of waiting it out.
ACTIVE_CONTAINER=""
HELD_LOCK=0

# --- diagnostics -------------------------------------------------------

die() {
  echo "" >&2
  echo "release-dry-run FAILED at stage: ${STAGE}" >&2
  echo "  ${1}" >&2
  exit 1
}

log_stage() {
  STAGE="$1"
  echo ""
  echo "=== release-dry-run: ${STAGE} ==="
}

# --rm containers clean themselves up on normal exit; this trap also sweeps
# up anything left behind by an interrupted (killed) run, so no manual
# `docker system prune` is ever required.
cleanup() {
  # Kill the in-progress container by name first. The prefix sweep below would
  # eventually catch it too, but on an interrupt this is the one call standing
  # between the user and a container that keeps building for another 15 minutes,
  # so it must not wait on a `docker ps` round trip.
  if [ -n "${ACTIVE_CONTAINER}" ]; then
    docker rm -f "${ACTIVE_CONTAINER}" >/dev/null 2>&1 || true
    ACTIVE_CONTAINER=""
  fi

  local leftover
  leftover="$(docker ps -aq --filter "name=${CONTAINER_PREFIX}" 2>/dev/null || true)"
  if [ -n "${leftover}" ]; then
    # Word-splitting is intentional: leftover is a newline-separated list of
    # container ids, all passed to one rm -f.
    # shellcheck disable=SC2086
    docker rm -f ${leftover} >/dev/null 2>&1 || true
  fi

  # Only the run that took the lock may drop it, or a second invocation exiting
  # on the "already in progress" path would release the first run's lock.
  if [ "${HELD_LOCK}" -eq 1 ]; then
    rmdir "${LOCK_DIR}" 2>/dev/null || true
    HELD_LOCK=0
  fi
}

# Distinct from die(): an aborted run and a broken run must not look the same in
# the terminal or to a caller reading $?. Exiting here also runs the EXIT trap,
# so cleanup happens exactly once.
on_signal() {
  echo "" >&2
  echo "release-dry-run INTERRUPTED by ${1} at stage: ${STAGE}" >&2
  exit "$2"
}

trap cleanup EXIT
trap 'on_signal SIGINT 130' INT
trap 'on_signal SIGTERM 143' TERM

mkdir -p "${WORKDIR}"
# mkdir is atomic, and unlike flock it exists on macOS as well as Linux. The
# per-run paths above stop two runs colliding on the layout, but they share one
# cargo target dir by necessity, so overlapping runs still have to be excluded.
if ! mkdir "${LOCK_DIR}" 2>/dev/null; then
  die "another release-dry-run appears to be in progress (lock: ${LOCK_DIR}). If no run is active, remove that directory and retry."
fi
HELD_LOCK=1

mkdir -p "${CACHE_CARGO}" "${CACHE_RUSTUP}"
rm -rf "${DEB_LAYOUT}"
mkdir -p "${DEB_LAYOUT}"

# Runs one stage's container as a background job and blocks in `wait`. A shell
# blocked directly on a foreground command defers its traps until that command
# returns, which on the build stage means up to 20 minutes between Ctrl-C and
# anything happening; a shell blocked in `wait` runs them immediately.
run_docker() {
  local name="$1"
  shift
  local rc=0
  ACTIVE_CONTAINER="${CONTAINER_PREFIX}-${name}"
  docker run --rm --name "${ACTIVE_CONTAINER}" --platform "${DOCKER_PLATFORM}" "$@" &
  wait "$!" || rc=$?
  ACTIVE_CONTAINER=""
  return "${rc}"
}

# --- stage 1: build inside the glibc floor container --------------------
#
# Mirrors release.yml lines 55-117: git/curl/build tooling installed fresh
# (nothing is preinstalled in the base image), rustup stable, then a release
# build with the same feature set the Linux legs ship with. Building on the
# host's own userland instead of debian:11 is exactly the mistake this
# script exists to catch -- it would silently raise the glibc floor.
build_in_floor_container() {
  log_stage "build (${BUILD_IMAGE}, target ${TARGET})"
  run_docker build \
    -v "${REPO_ROOT}:/repo" \
    -v "${REPO_ROOT}/${CACHE_CARGO}:/root/.cargo" \
    -v "${REPO_ROOT}/${CACHE_RUSTUP}:/root/.rustup" \
    -w /repo \
    "${BUILD_IMAGE}" bash -euc '
      set -euo pipefail
      apt-get update -qq
      apt-get install -y -qq --no-install-recommends \
        git curl ca-certificates build-essential pkg-config libdbus-1-dev binutils \
        cmake xz-utils
      if [ ! -f /opt/vulkansdk/'"${VULKAN_SDK_VERSION}"'/x86_64/bin/glslc ]; then
        curl -fsSL -o /tmp/vulkansdk.tar.xz \
          "https://sdk.lunarg.com/sdk/download/'"${VULKAN_SDK_VERSION}"'/linux/vulkansdk-linux-x86_64-'"${VULKAN_SDK_VERSION}"'.tar.xz"
        echo "'"${VULKAN_SDK_SHA256}"'  /tmp/vulkansdk.tar.xz" | sha256sum -c -
        mkdir -p /opt/vulkansdk
        tar -xf /tmp/vulkansdk.tar.xz -C /opt/vulkansdk
        rm /tmp/vulkansdk.tar.xz
      fi
      export VULKAN_SDK=/opt/vulkansdk/'"${VULKAN_SDK_VERSION}"'/x86_64
      export PATH="$VULKAN_SDK/bin:$PATH"
      if [ ! -x "$HOME/.cargo/bin/cargo" ]; then
        curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
          | sh -s -- -y --profile minimal --default-toolchain stable
      fi
      export PATH="$HOME/.cargo/bin:$PATH"
      # Single-quoted: $ORIGIN must reach the linker literally (release.yml
      # sets the same rpath so the shipped binary finds its llama libs beside
      # itself, or in ../lib/inkentry for the .deb split layout).
      export RUSTFLAGS="-C link-arg=-Wl,-rpath,\$ORIGIN:\$ORIGIN/../lib/inkentry"
      cargo build --release --target '"${TARGET}"' --features '"${FEATURES}"'
      strip "target/'"${TARGET}"'/release/inkentry"
      strip "target/'"${TARGET}"'/release/inkentry-server"
      scripts/collect-ggml-backends.sh \
        "target/'"${TARGET}"'/release" "target/'"${TARGET}"'/release"
      strip --strip-unneeded target/'"${TARGET}"'/release/lib*.so*
    ' || die "container build failed (see docker output above)"

  for bin in inkentry inkentry-server; do
    [ -x "target/${TARGET}/release/${bin}" ] || die "expected binary target/${TARGET}/release/${bin} not found after build"
  done
  ls "target/${TARGET}/release"/lib*.so* >/dev/null 2>&1 \
    || die "expected llama runtime libraries next to the binaries after build"
}

# --- stage 2: glibc ceiling check ---------------------------------------
#
# Lifted near-verbatim from release.yml lines 125-144. A missing, empty, or
# non-numeric ceiling is a failure, not a silent pass.
enforce_glibc_ceiling() {
  log_stage "glibc ceiling check (floor: GLIBC_2.31)"
  run_docker glibc-check \
    -v "${REPO_ROOT}:/repo:ro" \
    -w /repo \
    "${BUILD_IMAGE}" bash -euc '
      set -euo pipefail
      apt-get update -qq
      apt-get install -y -qq --no-install-recommends binutils >/dev/null
      shipped="inkentry inkentry-server $(cd "target/'"${TARGET}"'/release" && ls lib*.so*)"
      for bin in $shipped; do
        path="target/'"${TARGET}"'/release/${bin}"
        raw="$(objdump -T "$path")"
        ceiling="$(printf "%s\n" "$raw" | grep -o "GLIBC_[0-9.]*" | sort -Vu | tail -1 || true)"
        if ! [[ "$ceiling" =~ ^GLIBC_[0-9]+\.[0-9]+$ ]]; then
          echo "ERROR: ${bin}: no valid versioned GLIBC symbol found (got '"'"'${ceiling}'"'"')" >&2
          exit 1
        fi
        echo "${bin}: max versioned glibc symbol = ${ceiling}"
        max="$(printf "%s\nGLIBC_2.31\n" "$ceiling" | sort -V | tail -1)"
        if [ "$max" != "GLIBC_2.31" ]; then
          echo "ERROR: ${bin} requires ${ceiling}, above the glibc 2.31 floor" >&2
          exit 1
        fi
      done
    ' || die "glibc ceiling check failed -- a binary links against a glibc symbol newer than 2.31, or has no versioned GLIBC symbols at all"
}

# --- stage 3: assemble the .deb layout + derive Depends ------------------
#
# Depends is derived from the packaged binaries, never hardcoded, and
# derived INSIDE debian:11 (the build floor) -- release.yml lines 228-246
# explains why: deriving it on a newer-glibc host resolves to a Depends line
# that can't install on the floor. Layout assembly and control-file
# generation (via the existing write-deb-control.js -- pure Node builtins,
# nothing CI-specific) run on the host; only dpkg-shlibdeps itself needs the
# container.
assemble_deb() {
  log_stage "assemble .deb layout + derive Depends (${BUILD_IMAGE})"

  mkdir -p "${DEB_LAYOUT}/DEBIAN" "${DEB_LAYOUT}/usr/bin" "${DEB_LAYOUT}/usr/lib/systemd/user" \
    "${DEB_LAYOUT}/usr/lib/inkentry"
  install -m 755 "target/${TARGET}/release/inkentry" "${DEB_LAYOUT}/usr/bin/inkentry"
  install -m 755 "target/${TARGET}/release/inkentry-server" "${DEB_LAYOUT}/usr/bin/inkentry-server"
  install -m 644 "target/${TARGET}/release/"lib*.so* "${DEB_LAYOUT}/usr/lib/inkentry/"
  install -m 644 packaging/inkentry-server.service "${DEB_LAYOUT}/usr/lib/systemd/user/inkentry-server.service"

  # Captured via a file rather than command substitution: `$(...)` runs the
  # docker client in a subshell this script cannot background or `wait` on, and
  # keeping it would leave this one stage unable to respond to a signal.
  local deb_depends
  local depends_out="${RUN_DIR}/shlibdeps.txt"
  run_docker shlibdeps \
    -v "${REPO_ROOT}:/w:ro" "${BUILD_IMAGE}" bash -euc '
      set -euo pipefail
      apt-get update -qq >/dev/null
      apt-get install -y -qq --no-install-recommends dpkg-dev libdbus-1-3 >/dev/null
      mkdir -p /tmp/sd/debian
      printf "Source: inkentry\nMaintainer: inkentries <hello@inkentry.com>\n\nPackage: inkentry\nArchitecture: amd64\nDescription: placeholder\n placeholder\n" > /tmp/sd/debian/control
      cd /tmp/sd
      # -l resolves the private libggml/libllama libs; -e folds the core
      # libs own needs (libstdc++6, libgcc) into Depends. The ggml backend
      # MODULES stay out: dlopen-optional, and listing ggml-vulkan would
      # make libvulkan1 a hard dependency the CPU fallback does not have.
      dpkg-shlibdeps -O -l"/w/'"${DEB_LAYOUT}"'/usr/lib/inkentry" \
        -e"/w/'"${DEB_LAYOUT}"'/usr/lib/inkentry/libggml-base.so.0" \
        -e"/w/'"${DEB_LAYOUT}"'/usr/lib/inkentry/libggml.so.0" \
        -e"/w/'"${DEB_LAYOUT}"'/usr/lib/inkentry/libllama.so.0" \
        -e"/w/'"${DEB_LAYOUT}"'/usr/lib/inkentry/libllama-common.so.0" \
        "/w/'"${DEB_LAYOUT}"'/usr/bin/inkentry" "/w/'"${DEB_LAYOUT}"'/usr/bin/inkentry-server"
    ' >"${depends_out}" || die "dpkg-shlibdeps failed inside ${BUILD_IMAGE}"
  deb_depends="$(cat "${depends_out}")"
  [ -n "${deb_depends}" ] || die "dpkg-shlibdeps produced no Depends line"
  echo "Derived ${deb_depends}"

  DEB_DEPENDS="${deb_depends}" \
    node .github/scripts/write-deb-control.js \
    --deb-version "${DEB_VERSION}" \
    --out "${DEB_LAYOUT}/DEBIAN/control" \
    || die "write-deb-control.js failed"
}

# --- stage 4: build the .deb ---------------------------------------------
#
# -Zxz, not the host/container's default compressor: debian:11's dpkg (1.20)
# cannot read zstd-compressed control/data members, so a default-built .deb
# fails to install on the floor with "unknown compression for member". dpkg
# itself (and dpkg-deb) ships in every debian base image, so no extra
# package install is needed here.
build_deb() {
  log_stage "build .deb (-Zxz)"
  run_docker dpkg-deb \
    -v "${REPO_ROOT}:/w" -w /w \
    "${BUILD_IMAGE}" \
    dpkg-deb --build -Zxz "${DEB_LAYOUT}" "${DEB_PATH}" \
    || die "dpkg-deb --build failed"
  [ -f "${DEB_PATH}" ] || die "expected .deb not found at ${DEB_PATH} after dpkg-deb --build"
}

# --- stage 5: install + smoke-test on the floor (and current) images -----
#
# apt-get install succeeds on a .deb whose Depends omits a linked library;
# only executing real subcommands (a git-backed memory round trip included)
# surfaces that gap and proves the installed binary runs, not just links.
# INKENTRY_SECRET_STORE=file keeps the smoke test from touching a keychain
# inside the container. The scratch git repo lives in the container's own
# filesystem, never in this checkout.
smoke_test_deb() {
  for image in ${SMOKE_IMAGES}; do
    log_stage "install + smoke-test .deb (${image})"
    run_docker smoke \
      -v "${REPO_ROOT}/${DEB_PATH}:/pkg/inkentry_${DEB_VERSION}_amd64.deb:ro" \
      -e INKENTRY_SECRET_STORE=file \
      "${image}" bash -euc '
        set -euo pipefail
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        apt-get install -y -qq "/pkg/inkentry_'"${DEB_VERSION}"'_amd64.deb"
        apt-get install -y -qq --no-install-recommends git ca-certificates
        test -n "$(inkentry --version)"
        test -n "$(inkentry-server --version)"

        mkdir -p /w && cd /w
        git init -q .
        git config user.email t@t
        git config user.name t
        echo "fn main() {}" > main.rs
        git add . && git commit -qm init

        inkentry status
        inkentry init --no-index
        inkentry memory add --kind note --title "deb smoke" --body "runs on this image"
        inkentry memory list | grep -q "deb smoke"
      ' || die "install/smoke-test failed on ${image}"
  done
}

# Explicitly does not touch: `gh release create`, any push to the
# `homebrew-inkentry` tap, or a write to `bucket/inkentry.json`. Grep this
# file -- there is no code path above that invokes any of the three.

main() {
  build_in_floor_container
  enforce_glibc_ceiling
  assemble_deb
  build_deb
  smoke_test_deb

  echo ""
  echo "=== release-dry-run: PASS ==="
  echo "Built and smoke-tested: ${DEB_PATH}"
  echo "This proves the Linux x86_64 build, glibc-2.31 floor, and .deb install/smoke are release-safe."
  echo "It does NOT exercise macOS/Windows builds, the GitHub Release, or the Homebrew/Scoop publish steps."
}

main "$@"
