#!/usr/bin/env bash
# bench/perf_index.sh — spelunk indexing throughput benchmark
#
# Times `spelunk index` on the project itself and reports files/second,
# chunks/second, and peak memory.
#
# Usage:
#   ./bench/perf_index.sh [REPO_PATH]
#
# Prerequisites:
#   - spelunk binary in PATH
#   - /usr/bin/time (macOS: install gtime via `brew install gnu-time`)

set -euo pipefail

SPELUNK="${SPELUNK:-spelunk}"
REPO="${1:-.}"
TIME_CMD="time"

# Prefer GNU time for memory reporting; fall back to built-in
if command -v gtime &>/dev/null; then
    TIME_CMD="gtime"
elif /usr/bin/time -l true 2>&1 | grep -q "maximum"; then
    # macOS /usr/bin/time supports -l
    TIME_CMD="time"
fi

echo "=== Indexing throughput ==="
echo "Repo:     ${REPO}"
echo "Binary:   ${SPELUNK}"
echo

# Clean any existing index so we measure a fresh index
rm -rf "${REPO}/.spelunk" 2>/dev/null || true

echo "Indexing..."

if [[ "$TIME_CMD" == "gtime" ]]; then
    # GNU time: -v for verbose
    $TIME_CMD -v "$SPELUNK" index "$REPO" 2>&1 | tee /tmp/spelunk-perf-index.log
    WALL=$(grep "Elapsed" /tmp/spelunk-perf-index.log | tail -1 | awk '{print $NF}' | tr -d ':')
    MAX_RSS=$(grep "Maximum resident" /tmp/spelunk-perf-index.log | tail -1 | awk '{print $NF}')
else
    # macOS /usr/bin/time
    $TIME_CMD -l "$SPELUNK" index "$REPO" 2>&1 | tee /tmp/spelunk-perf-index.log
    WALL=$(grep "real" /tmp/spelunk-perf-index.log | tail -1 | awk '{print $2}')
    MAX_RSS=$(grep "maximum resident" /tmp/spelunk-perf-index.log | tail -1 | awk '{print $1}')
fi

# Extract stats from spelunk output
FILES=$(grep -o '[0-9,]* files' /tmp/spelunk-perf-index.log | tail -1 | grep -o '[0-9,]*' | tr -d ',' || echo "?")
CHUNKS=$(grep -o '[0-9,]* chunks' /tmp/spelunk-perf-index.log | tail -1 | grep -o '[0-9,]*' | tr -d ',' || echo "?")
EMBEDS=$(grep -o '[0-9,]* embeddings' /tmp/spelunk-perf-index.log | tail -1 | grep -o '[0-9,]*' | tr -d ',' || echo "?")

# Parse wall time (format: "0m12.34s" or "12.34")
WALL_SEC=$(echo "$WALL" | sed 's/m/ /' | awk '{split($0,a," "); print a[1]*60 + a[2]}' 2>/dev/null || echo "$WALL")

echo
echo "=== Results ==="
echo "Files:      ${FILES}"
echo "Chunks:     ${CHUNKS}"
echo "Embeddings: ${EMBEDS}"
echo "Wall time:  ${WALL_SEC}s"
echo "Peak RSS:   ${MAX_RSS} KB"

if [[ "$FILES" != "?" && "$WALL_SEC" != "?" && "$WALL_SEC" != "0" ]]; then
    FPS=$(python3 -c "print(f'{$FILES / $WALL_SEC:.1f}')")
    echo "Throughput: ${FPS} files/s"
fi

rm -f /tmp/spelunk-perf-index.log
