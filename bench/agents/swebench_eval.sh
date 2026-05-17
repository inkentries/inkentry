#!/usr/bin/env bash
# bench/agents/swebench_eval.sh — run SWE-bench Docker evaluation
#
# Converts agent patches to SWE-bench prediction format and runs the
# official Docker harness to compute resolve_rate.
#
# Prerequisites:
#   - SWE-bench harness installed: pip install swebench
#   - Docker images pulled for the target dataset
#   - Agent patches saved via agent.py --save-patch
#
# Usage:
#   bash bench/agents/swebench_eval.sh \\
#       --results bench/results/swebench-baseline-batch.json \\
#       --patches-dir bench/patches/baseline \\
#       --dataset princeton-nlp/SWE-bench_Verified \\
#       --split test
#
# Options:
#   --results FILE     batch result JSON from agent run
#   --patches-dir DIR  directory with per-task .patch files
#   --dataset NAME     HuggingFace dataset (default: princeton-nlp/SWE-bench_Verified)
#   --split NAME       dataset split (default: test)
#   --max-workers N    parallel eval workers (default: 4)
#   --timeout SEC      per-instance timeout (default: 900)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

RESULTS=""
PATCHES_DIR=""
DATASET="princeton-nlp/SWE-bench_Verified"
SPLIT="test"
MAX_WORKERS=4
TIMEOUT=900

usage() {
    grep '^#' "$0" | grep -v '#!/' | sed 's/^# \?//'
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --results)      RESULTS="$2";      shift 2 ;;
        --patches-dir)  PATCHES_DIR="$2";   shift 2 ;;
        --dataset)      DATASET="$2";       shift 2 ;;
        --split)        SPLIT="$2";         shift 2 ;;
        --max-workers)  MAX_WORKERS="$2";   shift 2 ;;
        --timeout)      TIMEOUT="$2";       shift 2 ;;
        -h|--help)      usage ;;
        *) echo "Unknown argument: $1" >&2; usage ;;
    esac
done

if [[ -z "$RESULTS" || -z "$PATCHES_DIR" ]]; then
    echo "Error: --results and --patches-dir are required." >&2; usage
fi

TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
PREDICTIONS_FILE="${SCRIPT_DIR}/../predictions/eval-${TIMESTAMP}.json"
mkdir -p "$(dirname "$PREDICTIONS_FILE")"

echo "=== SWE-bench Evaluation ==="
echo "Results:      ${RESULTS}"
echo "Patches:      ${PATCHES_DIR}"
echo "Dataset:      ${DATASET}"
echo "Predictions:  ${PREDICTIONS_FILE}"
echo ""

# Step 1: Export patches to SWE-bench format
echo "--- Exporting patches ---"
python3 "${SCRIPT_DIR}/export_patches.py" \
    --results "$RESULTS" \
    --patches-dir "$PATCHES_DIR" \
    --out "$PREDICTIONS_FILE"

# Step 2: Extract condition from metadata sidecar for run_id
CONDITION="unknown"
META_FILE="${PREDICTIONS_FILE%.json}.meta.json"
if [[ -f "$META_FILE" ]]; then
    CONDITION=$(python3 -c "import json; print(json.load(open('${META_FILE}')).get('condition','unknown'))" 2>/dev/null || echo "unknown")
fi
RUN_ID="spelunk-${CONDITION}-${TIMESTAMP}"

echo ""
echo "--- Running Docker evaluation ---"
python3 -m swebench.harness.run_evaluation \
    --dataset_name "$DATASET" \
    --split "$SPLIT" \
    --predictions_path "$PREDICTIONS_FILE" \
    --max_workers "$MAX_WORKERS" \
    --timeout "$TIMEOUT" \
    --run_id "$RUN_ID"

# Step 3: Merge harness results back into result JSON
echo ""
echo "--- Merging resolve data ---"
HARNESS_DIR="swebench_eval_outputs/${RUN_ID}"
if [[ -d "$HARNESS_DIR" ]]; then
    python3 -c "
import json, sys
from pathlib import Path

results = json.load(open('${RESULTS}'))
harness = json.load(open('${HARNESS_DIR}/results.json')) if Path('${HARNESS_DIR}/results.json').exists() else {}

resolved_map = {}
for r in harness.get('resolved', []):
    resolved_map[r] = True

for r in results:
    tid = r.get('task_id','')
    r['resolved'] = resolved_map.get(tid, False)

tasks_evaluated = len([r for r in results if not r.get('skipped') and not r.get('error')])
tasks_resolved = sum(1 for r in results if r.get('resolved'))
print(f'  Evaluated: {tasks_evaluated}  Resolved: {tasks_resolved}')

json.dump(results, open('${RESULTS}','w'), indent=2)
"
fi

echo ""
echo "=== Done ==="
echo "Results in: ${HARNESS_DIR}/"
