#!/usr/bin/env bash
#
# Version-skew smoke test: drive one real `spelunk` binary against one real
# `spelunk-server` binary through the end-to-end memory flow.
#
#   usage: skew-smoke.sh <path-to-spelunk> <path-to-spelunk-server>
#
# Every other contract test in this repo talks to a mock or a fixture written
# to the shape we *believe* a peer has. This script is the only one that puts
# two independently built artifacts on a socket together, so it is the only one
# that can falsify that belief. CI runs it in both directions: current CLI
# against the previous released server, and the previous released CLI against
# the current server.
#
# See docs/version-skew.md for the support window this is asserting.

set -euo pipefail

CLI_BIN="${1:?usage: skew-smoke.sh <spelunk> <spelunk-server>}"
SERVER_BIN="${2:?usage: skew-smoke.sh <spelunk> <spelunk-server>}"

for bin in "$CLI_BIN" "$SERVER_BIN"; do
  [ -x "$bin" ] || { echo "FAIL: not an executable: $bin" >&2; exit 1; }
done

# The released binaries pre-date the keychain fix and will block on a real
# macOS Keychain prompt without this. It is exported rather than passed per
# command so every child the CLI spawns inherits it too.
export SPELUNK_SECRET_STORE=file

WORK="$(mktemp -d)"
SERVER_PID=""

cleanup() {
  # Only ever kills the server this script started. A developer box may well
  # have its own spelunk-server on the default port; that one is not ours.
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

# Bind an ephemeral port and immediately release it. Racy in principle; in
# practice the server binds within milliseconds and a collision fails loudly on
# the health check below rather than silently passing.
free_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

PORT="$(free_port)"
BASE="http://127.0.0.1:${PORT}"

echo "== starting server: $SERVER_BIN on $BASE"
"$SERVER_BIN" --port "$PORT" --db "$WORK/server.db" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!

HEALTH=""
for _ in $(seq 1 40); do
  HEALTH="$(curl -sf -m 2 "$BASE/v1/health" || true)"
  [ -n "$HEALTH" ] && break
  sleep 1
done
[ -n "$HEALTH" ] || { cat "$WORK/server.log" >&2; fail "server never answered /v1/health"; }

SERVER_VERSION="$(printf '%s' "$HEALTH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["version"])')"
CLI_VERSION="$("$CLI_BIN" --version | awk '{print $NF}')"
echo "== CLI $CLI_VERSION  <->  server $SERVER_VERSION"

# Without this the whole job can go green while proving nothing: point both
# arguments at the same build and every assertion below still passes. A skew
# test that is not skewed is not a test.
[ "$CLI_VERSION" != "$SERVER_VERSION" ] \
  || fail "CLI and server are both $CLI_VERSION; this run tested no skew at all"

# A fixed slug shared by both checkouts below, so the second one pulls exactly
# what the first one pushed. Left to `spelunk init` it would be a per-directory
# content hash and the pull would legitimately find nothing.
PROJECT_ID="local/skewsmoke"

# Isolated HOME: never touch the invoking user's config, registry, or keyring.
export HOME="$WORK/home"
export XDG_CONFIG_HOME="$WORK/home/.config"
export XDG_STATE_HOME="$WORK/home/.local/state"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_STATE_HOME"

# Explicit server URL, so the CLI talks to the binary under test and never
# auto-discovers some other server already listening on the default port.
export SPELUNK_SERVER_URL="$BASE"

make_project() {
  local dir="$1"
  mkdir -p "$dir"
  git -C "$dir" init -q .
  git -C "$dir" config user.email skew@example.invalid
  git -C "$dir" config user.name "skew smoke"
  echo 'fn main() {}' >"$dir/main.rs"
  git -C "$dir" add -A
  git -C "$dir" -c commit.gpgsign=false commit -qm "initial"
  mkdir -p "$dir/.spelunk"
  printf 'project_id = "%s"\n' "$PROJECT_ID" >"$dir/.spelunk/config.toml"
}

run() {
  local label="$1"; shift
  echo "-- $label"
  # Exit status is read straight off the command. Piping it into tee or tail
  # would report the pipeline's status instead, which is always the last stage.
  if ! ( cd "$WORK/a" && "$@" ) >"$WORK/$label.out" 2>&1; then
    cat "$WORK/$label.out" >&2
    fail "$label exited non-zero (CLI $CLI_VERSION -> server $SERVER_VERSION)"
  fi
}

make_project "$WORK/a"

run add-decision "$CLI_BIN" memory add -k decision \
  -t "Skew smoke decision" -b "Written by the CLI at version $CLI_VERSION."
run add-note "$CLI_BIN" memory add -k note \
  -t "Skew smoke note" -b "A second entry, so list and pull counts are not trivially one."

run list "$CLI_BIN" memory list
grep -q "Skew smoke decision" "$WORK/list.out" || fail "memory list lost the decision entry"
grep -q "Skew smoke note" "$WORK/list.out" || fail "memory list lost the note entry"

# Search is the one step whose outcome depends on something other than the
# wire contract: the server embeds the query, so it needs the model loaded.
# Wait for the embedder to settle before judging the result, otherwise this
# step measures model download speed rather than version skew. An earlier
# draft of this script did exactly that and produced a convincing false
# positive: an old CLI "failing" against a new server purely because the new
# server was a debug build and was still warming up.
echo "-- waiting for embedder to settle"
EMBEDDER_STATE="unknown"
for _ in $(seq 1 "${SKEW_EMBEDDER_TIMEOUT_SECS:-300}"); do
  EMBEDDER_STATE="$(curl -sf -m 2 "$BASE/v1/health" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin).get("embedder",{}).get("state","unknown"))' \
    2>/dev/null || echo unknown)"
  case "$EMBEDDER_STATE" in
    ready|unavailable|disabled) break ;;
  esac
  sleep 1
done
echo "   embedder state: $EMBEDDER_STATE"

echo "-- search"
if ( cd "$WORK/a" && "$CLI_BIN" memory search "skew smoke decision" ) >"$WORK/search.out" 2>&1; then
  grep -q "Skew smoke decision" "$WORK/search.out" \
    || { cat "$WORK/search.out" >&2; fail "memory search succeeded but did not surface the decision entry"; }
elif [ "$EMBEDDER_STATE" = "ready" ]; then
  cat "$WORK/search.out" >&2
  fail "memory search failed against a ready embedder (CLI $CLI_VERSION -> server $SERVER_VERSION)"
else
  # Not a pass for search, but not a skew failure either. The one thing that
  # must still hold is that the refusal is the *documented* one: an old client
  # and a new server still agreeing on the shape of a not-ready error is
  # itself part of the contract under test. A protocol-level failure here
  # (404, 405, a deserialization error) is a real break and must not be
  # waved through by this branch.
  grep -Eqi 'embedder|embedding model|warming up|503' "$WORK/search.out" \
    || { cat "$WORK/search.out" >&2; fail "memory search failed for a reason unrelated to embedder readiness"; }
  echo "   search skipped: embedder never became ready (state=$EMBEDDER_STATE), refusal was the documented one"
fi

run push "$CLI_BIN" memory push
grep -q "created 2" "$WORK/push.out" \
  || { cat "$WORK/push.out" >&2; fail "push did not report 2 created entries across the skew boundary"; }

# Re-push must be idempotent on external_id. A server that lost that dedupe
# would look identical to a working one on the first push alone.
run repush "$CLI_BIN" memory push
grep -q "already synced" "$WORK/repush.out" \
  || { cat "$WORK/repush.out" >&2; fail "re-push was not idempotent"; }

run sync "$CLI_BIN" memory sync

# The real assertion. A second, empty checkout of the same project must be able
# to read back what the first one wrote, which exercises the response half of
# the wire contract rather than just the request half.
make_project "$WORK/b"
echo "-- pull-into-fresh-checkout"
if ! ( cd "$WORK/b" && "$CLI_BIN" memory pull ) >"$WORK/pull.out" 2>&1; then
  cat "$WORK/pull.out" >&2
  fail "pull into a fresh checkout exited non-zero"
fi

if ! ( cd "$WORK/b" && "$CLI_BIN" memory list ) >"$WORK/list-b.out" 2>&1; then
  cat "$WORK/list-b.out" >&2
  fail "memory list in the fresh checkout exited non-zero"
fi
grep -q "Skew smoke decision" "$WORK/list-b.out" \
  || { cat "$WORK/list-b.out" >&2; fail "decision entry did not survive the push/pull round trip"; }
grep -q "Skew smoke note" "$WORK/list-b.out" \
  || { cat "$WORK/list-b.out" >&2; fail "note entry did not survive the push/pull round trip"; }

echo "PASS: CLI $CLI_VERSION <-> server $SERVER_VERSION completed the memory flow"
