#!/usr/bin/env bash
# bench/git_meta_perf.sh — performance benchmark for GitMetaBackend (default)
#
# Creates a synthetic git repo with ~5000 commits and populates spelunk memory
# via `memory add` at 100% density (every commit), then times
# `spelunk memory list --kind decision --limit 10`.
#
# Target: <100ms on a 5k-commit repo at 100% note density.
#
# Usage:
#   ./bench/git_meta_perf.sh [COMMITS]
#
# Prerequisites:
#   - spelunk binary in PATH
#   - git
#   - A spelunk index of the fixture repo (`spelunk index .`)

set -euo pipefail

COMMITS="${1:-5000}"
SPELUNK="${SPELUNK:-spelunk}"

# ── locate binary ──────────────────────────────────────────────────────────
if ! command -v "$SPELUNK" &>/dev/null; then
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
echo "Parameters: COMMITS=$COMMITS (100% density)"
echo

# ── create fixture repo ────────────────────────────────────────────────────
FIXTURE_DIR="$(mktemp -d)"
echo "Fixture repo: $FIXTURE_DIR"
trap 'rm -rf "$FIXTURE_DIR"' EXIT

cd "$FIXTURE_DIR"
git init -b main -q
git config user.email "bench@example.com"
git config user.name "Bench"

echo "benchmark fixture for GitMetaBackend" > README.md
mkdir -p src
echo "fn main() { println!(\"hello\"); }" > src/main.rs
git add .
git commit --no-gpg-sign -q -m "init"

echo "Creating $COMMITS commits…"
for i in $(seq 1 "$COMMITS"); do
    echo "// commit $i" >> src/main.rs
    git add src/main.rs >/dev/null 2>&1
    git commit --no-gpg-sign -q --allow-empty -m "commit $i" >/dev/null 2>&1
done
echo "Created $COMMITS commits."

# ── index the repo (required for memory) ────────────────────────────────────
echo "Indexing repo..."
"$SPELUNK" index "$FIXTURE_DIR" 2>/dev/null

# ── populate memory at 100% density ─────────────────────────────────────────
echo "Populating memory ($COMMITS notes)..."
for i in $(seq 1 "$COMMITS"); do
    "$SPELUNK" memory add \
        --kind decision \
        --title "decision $i" \
        --body "Body for commit $i. This is a synthetic decision entry for benchmarking GitMetaBackend at scale." \
        --tags "bench,perf" \
        2>/dev/null
done
echo "Populated $COMMITS notes."

# ── benchmarks ──────────────────────────────────────────────────────────────
ms_now() {
    python3 -c "import time; print(int(time.time()*1000))"
}

run_timed() {
    local label="$1"; shift
    local start end elapsed_ms

    start=$(ms_now)
    ( cd "$FIXTURE_DIR" && "$SPELUNK" memory "$@" > /dev/null )
    end=$(ms_now)
    elapsed_ms=$(( end - start ))

    printf "%-55s %4dms\n" "$label" "$elapsed_ms"

    if [[ "$label" == *"(primary)"* ]] && (( elapsed_ms > 100 )); then
        echo "  ⚠️  EXCEEDS 100ms target"
    fi
}

echo
echo "=== Benchmark results ==="
run_timed "memory list --kind decision --limit 10  (primary)" \
    list --kind decision --limit 10

run_timed "memory list --limit 10" \
    list --limit 10

run_timed "memory search 'benchmark' --limit 5" \
    search "benchmark" --limit 5

echo
echo "=== Notes ==="
echo "  $COMMITS notes at 100% density on $COMMITS commits"
echo "  100ms is the target for 'list --kind decision --limit 10'."
echo "  Compare with bench/git_notes_perf.sh results for backend selection."
