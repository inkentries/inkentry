#!/usr/bin/env bash
# bench/git_notes_perf.sh — performance benchmark for GitNotesBackend
#
# Creates a synthetic git repo with ~5000 commits and ~50 spelunk notes
# (1% density), then times `spelunk memory list --backend git-notes`.
#
# Success criterion (from issue #186): <500ms for `list --limit 10` on a
# 5k-commit repo.
#
# Usage:
#   ./bench/git_notes_perf.sh [COMMITS] [NOTE_PCT]
#
# Examples:
#   ./bench/git_notes_perf.sh            # default: 5000 commits, 1% notes
#   ./bench/git_notes_perf.sh 1000 5     # 1000 commits, 5% notes
#
# Prerequisites:
#   - spelunk binary in PATH (cargo install --path . or cargo build --release)
#   - git

set -euo pipefail

COMMITS="${1:-5000}"
NOTE_PCT="${2:-1}"
SPELUNK="${SPELUNK:-spelunk}"

# ── locate binary ────────────────────────────────────────────────────────────
if ! command -v "$SPELUNK" &>/dev/null; then
    # Try the release binary in the cargo target directory
    REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel 2>/dev/null || echo .)"
    if [ -x "$REPO_ROOT/target/release/spelunk" ]; then
        SPELUNK="$REPO_ROOT/target/release/spelunk"
    elif [ -x "$REPO_ROOT/target/debug/spelunk" ]; then
        SPELUNK="$REPO_ROOT/target/debug/spelunk"
    else
        echo "ERROR: spelunk not found. Build with: cargo build --release" >&2
        exit 1
    fi
fi

echo "Using: $SPELUNK"
echo "Parameters: COMMITS=$COMMITS, NOTE_PCT=$NOTE_PCT"
echo

# ── create fixture repo ──────────────────────────────────────────────────────
FIXTURE_DIR="$(mktemp -d)"
echo "Fixture repo: $FIXTURE_DIR"
trap 'rm -rf "$FIXTURE_DIR"' EXIT

cd "$FIXTURE_DIR"
git init -b main -q
git config user.email "bench@example.com"
git config user.name "Bench"

# Create initial commit
echo "benchmark fixture" > README.md
git add .
git commit --no-gpg-sign -q -m "init"

NOTE_INTERVAL=$(( 100 / NOTE_PCT ))  # attach a note every N commits
NOTES_WRITTEN=0

echo "Creating $COMMITS commits (attaching a note every $NOTE_INTERVAL commits)…"

for i in $(seq 1 "$COMMITS"); do
    echo "$i" > "file_$i.txt"
    git add . -q
    git commit --no-gpg-sign -q --allow-empty -m "commit $i" 2>/dev/null

    if (( i % NOTE_INTERVAL == 0 )); then
        SHA=$(git rev-parse HEAD)
        NOTE_JSON="{\"id\":$i,\"kind\":\"decision\",\"title\":\"decision $i\",\"body\":\"body for commit $i\",\"tags\":[],\"linked_files\":[],\"created_at\":$(date +%s),\"status\":\"active\"}"
        git notes --ref=spelunk add -f -m "$NOTE_JSON" "$SHA" 2>/dev/null
        NOTES_WRITTEN=$((NOTES_WRITTEN + 1))
    fi
done

echo "Created $COMMITS commits, attached $NOTES_WRITTEN notes."
echo

# ── benchmarks ───────────────────────────────────────────────────────────────
run_timed() {
    local label="$1"; shift
    local start end elapsed_ms

    start=$(date +%s%3N 2>/dev/null || python3 -c "import time; print(int(time.time()*1000))")
    "$SPELUNK" "$@" --backend git-notes > /dev/null
    end=$(date +%s%3N 2>/dev/null || python3 -c "import time; print(int(time.time()*1000))")
    elapsed_ms=$(( end - start ))

    printf "%-55s %4dms\n" "$label" "$elapsed_ms"

    if [[ "$label" == *"(primary)"* ]] && (( elapsed_ms > 500 )); then
        echo "  ⚠️  EXCEEDS 500ms threshold — see issue #186 for mitigations"
    fi
}

echo "=== Benchmark results ==="
run_timed "memory list --kind decision --limit 10  (primary)" \
    -C "$FIXTURE_DIR" memory list --kind decision --limit 10

run_timed "memory list --limit 10" \
    -C "$FIXTURE_DIR" memory list --limit 10

run_timed "memory list  (no limit, worst-case)" \
    -C "$FIXTURE_DIR" memory list --limit 9999

echo
echo "=== Notes ==="
echo "  $NOTES_WRITTEN notes across $COMMITS commits ($NOTE_PCT% density)"
echo "  500ms is the target for 'list --limit 10' on a 5k-commit repo."
echo
echo "  Mitigation options if threshold is exceeded:"
echo "  A) Index file: maintain .git/notes/spelunk-index.ndjson"
echo "  B) Reverse log with early exit: --max-count=<limit*20> + early-exit"
echo "  C) Ref-per-kind: store under refs/notes/spelunk/<kind>/<sha>"
