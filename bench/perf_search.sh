#!/usr/bin/env bash
# bench/perf_search.sh — spelunk search latency benchmark
#
# Times `spelunk search` across different query lengths and modes,
# reporting p50/p95/p99 latency.
#
# Usage:
#   ./bench/perf_search.sh [REPO_PATH]
#
# Prerequisites:
#   - Repo must already be indexed
#   - python3 with statistics module

set -euo pipefail

SPELUNK="${SPELUNK:-spelunk}"
REPO="${1:-.}"

echo "=== Search latency ==="
echo "Repo:   ${REPO}"
echo "Binary: ${SPELUNK}"
echo

# Queries of varying lengths
QUERIES=(
    "error handling"
    "how does authentication work"
    "find all functions that parse command line arguments and validate user input"
)

MODES=("hybrid" "text")

for mode in "${MODES[@]}"; do
    echo "--- Mode: ${mode} ---"

    TIMES=()
    for q in "${QUERIES[@]}"; do
        echo "  Query: \"${q}\""
        ITER=10
        RUN_TIMES=()

        for _ in $(seq 1 $ITER); do
            start=$(python3 -c "import time; print(int(time.time()*1000))")
            "$SPELUNK" search "$q" --mode "$mode" --limit 5 --format json > /dev/null 2>&1
            end=$(python3 -c "import time; print(int(time.time()*1000))")
            elapsed=$(( end - start ))
            RUN_TIMES+=("$elapsed")
        done

        # Compute stats via python
        STATS=$(python3 -c "
import statistics
times = [$(echo "${RUN_TIMES[*]}" | tr ' ' ',')]
times.sort()
p50 = times[len(times)//2]
p95 = times[int(len(times)*0.95)]
p99 = times[min(int(len(times)*0.99), len(times)-1)]
mean = statistics.mean(times)
print(f'{p50},{p95},{p99},{mean:.1f}')
")
        P50=$(echo "$STATS" | cut -d, -f1)
        P95=$(echo "$STATS" | cut -d, -f2)
        P99=$(echo "$STATS" | cut -d, -f3)
        MEAN=$(echo "$STATS" | cut -d, -f4)

        printf "    p50=%4sms  p95=%4sms  p99=%4sms  mean=%5sms\n" "$P50" "$P95" "$P99" "$MEAN"
    done
    echo
done
