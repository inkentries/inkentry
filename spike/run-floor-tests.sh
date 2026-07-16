#!/usr/bin/env bash
# Spike: execute the bullseye-built binaries on the support-floor images and on
# current images, in fresh containers with no build tooling.
#
# Usage: run-floor-tests.sh <amd64|arm64> [image ...]
# Default image set: debian:11 ubuntu:20.04 ubuntu:24.04 debian:12
#
# Two phases per image:
#   phase 1: bare image: ldd + run (expected: libdbus-1.so.3 may be missing on
#            minimal images; that is a runtime Depends, satisfied by the .deb)
#   phase 2: after `apt-get install libdbus-1-3` (what the .deb's shlibdeps
#            Depends pulls in), then full exercise: --version, status, memory
#            add/list round trip with SPELUNK_SECRET_STORE=file.
set -uo pipefail

ARCH="${1:?usage: run-floor-tests.sh <amd64|arm64> [image ...]}"
shift || true
case "$ARCH" in
  amd64) PLATFORM=linux/amd64 ;;
  arm64) PLATFORM=linux/arm64 ;;
  *) echo "unknown arch: $ARCH" >&2; exit 2 ;;
esac
IMAGES=("$@")
[ ${#IMAGES[@]} -eq 0 ] && IMAGES=(debian:11 ubuntu:20.04 ubuntu:24.04 debian:12)

SRC="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$SRC/spike/out-$ARCH"
[ -x "$OUT/spelunk" ] || { echo "no binaries in $OUT; run build first" >&2; exit 2; }

overall=0
for IMG in "${IMAGES[@]}"; do
  echo
  echo "##### $IMG ($PLATFORM) #####"
  CN="${SPIKE_PREFIX:-spelunk-bullseye-spike}-run-$ARCH-$(echo "$IMG" | tr ':.' '--')"
  docker rm -f "$CN" >/dev/null 2>&1 || true
  docker run --name "$CN" --platform "$PLATFORM" \
    -v "$OUT:/bin-under-test:ro" \
    -e SPELUNK_SECRET_STORE=file \
    "$IMG" bash -uc '
      set +e
      echo "--- image glibc: $(ldd --version | head -1)"
      echo "--- phase 1: bare image ---"
      ldd /bin-under-test/spelunk 2>&1 | grep -E "not found|libdbus|libc.so|GLIBC" || true
      /bin-under-test/spelunk --version; echo "bare spelunk --version rc=$?"
      echo "--- phase 2: with runtime deps (libdbus-1-3, git, ca-certificates) ---"
      export DEBIAN_FRONTEND=noninteractive
      apt-get update -qq >/dev/null 2>&1
      apt-get install -y -qq --no-install-recommends libdbus-1-3 git ca-certificates >/dev/null 2>&1
      echo "libdbus-1-3: $(dpkg -s libdbus-1-3 2>/dev/null | grep ^Version || echo MISSING)"
      fails=0
      check() { # name rc
        if [ "$2" -ne 0 ]; then echo "FAIL: $1 (rc=$2)"; fails=$((fails+1)); else echo "OK: $1"; fi
      }
      ldd /bin-under-test/spelunk | grep -q "not found"; miss=$?
      [ $miss -eq 0 ] && { echo "FAIL: ldd unresolved:"; ldd /bin-under-test/spelunk | grep "not found"; fails=$((fails+1)); } || echo "OK: ldd spelunk fully resolved"
      ldd /bin-under-test/spelunk-server | grep -q "not found"; miss=$?
      [ $miss -eq 0 ] && { echo "FAIL: ldd server unresolved:"; ldd /bin-under-test/spelunk-server | grep "not found"; fails=$((fails+1)); } || echo "OK: ldd spelunk-server fully resolved"
      v="$(/bin-under-test/spelunk --version)"; check "spelunk --version -> $v" $?
      sv="$(/bin-under-test/spelunk-server --version)"; check "spelunk-server --version -> $sv" $?
      mkdir -p /w && cd /w
      git init -q . && git config user.email t@t && git config user.name t
      echo "fn main() {}" > main.rs
      git add . && git commit -qm init
      /bin-under-test/spelunk status; check "spelunk status" $?
      /bin-under-test/spelunk init --help >/dev/null; check "spelunk init --help" $?
      out="$(/bin-under-test/spelunk init --no-index 2>&1)"; rc=$?
      echo "$out" | tail -5
      check "spelunk init --no-index" $rc
      /bin-under-test/spelunk memory add --kind note --title "floor smoke" --body "runs on this image"; check "memory add" $?
      /bin-under-test/spelunk memory list | grep -q "floor smoke"; check "memory list round-trip" $?
      echo "IMAGE-RESULT fails=$fails"
      exit $fails
    '
  rc=$?
  docker rm -f "$CN" >/dev/null 2>&1 || true
  echo "##### $IMG rc=$rc #####"
  [ $rc -ne 0 ] && overall=1
done
exit $overall
