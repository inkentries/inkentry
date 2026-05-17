#!/usr/bin/env bash
# Run RepoBench-Python cross-file completion benchmark for Gemma.
#
# Normal usage (spelunk condition against committed baseline):
#   bash bench/gemma/crosscodeeval/run.sh --condition spelunk --repo-path /path/to/repo
#
# Regenerate the committed baseline:
#   bash bench/gemma/crosscodeeval/run.sh --condition baseline --samples 400
#
# Options:
#   --condition    baseline_single_shot|multi_turn_no_tools|naive_search|spelunk  (required)
#   --repo-path    PATH                      path to indexed repo (required for naive_search, spelunk)
#   --split        cross_file_first|cross_file_random|in_file  (default: cross_file_first)
#   --samples      N                         samples (default: 100)
#   --model        MODEL                     model name (default: deepseek-v4-flash)
#   --api-base-url URL                       (default: https://api.deepseek.com/v1)
#   --api-key      KEY                       API key (falls back to DEEPSEEK_API_KEY)
#   --out          FILE                      output path (default: bench/results/...)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
REPO_ROOT="$(cd "${BENCH_DIR}/.." && pwd)"
BASELINES_DIR="${REPO_ROOT}/baselines"
BASELINE_FILE="${BASELINES_DIR}/repobench-gemma-4-e2b-it-baseline.json"

usage() {
    grep '^#' "$0" | grep -v '#!/' | sed 's/^# \?//'
    exit 1
}

# Defaults
CONDITION=""
REPO_PATH=""
SPLIT="cross_file_first"
SAMPLES=200
MODEL="deepseek-v4-flash"
API_BASE_URL="https://api.deepseek.com/v1"
API_KEY="${DEEPSEEK_API_KEY:-}"
OUT_FILE=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --condition)    CONDITION="$2";    shift 2 ;;
        --repo-path)    REPO_PATH="$2";    shift 2 ;;
        --split)        SPLIT="$2";        shift 2 ;;
        --samples)      SAMPLES="$2";      shift 2 ;;
        --model)        MODEL="$2";        shift 2 ;;
        --api-base-url) API_BASE_URL="$2"; shift 2 ;;
        --api-key)      API_KEY="$2";      shift 2 ;;
        --out)          OUT_FILE="$2";     shift 2 ;;
        -h|--help)      usage ;;
        *) echo "Unknown argument: $1" >&2; usage ;;
    esac
done

if [[ -z "$CONDITION" ]]; then
    echo "Error: --condition is required." >&2; usage
fi
VALID=(baseline_single_shot multi_turn_no_tools naive_search spelunk)
if [[ ! " ${VALID[*]} " =~ " ${CONDITION} " ]]; then
    echo "Error: --condition must be one of: ${VALID[*]}" >&2; exit 1
fi

TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
if [[ -z "$OUT_FILE" ]]; then
    mkdir -p "${BENCH_DIR}/results"
    OUT_FILE="${BENCH_DIR}/results/repobench-${CONDITION}-${TIMESTAMP}.json"
fi

# Compute scaffold_hash from last commit touching bench/
SCAFFOLD_HASH="$(git -C "${REPO_ROOT}" log -1 --format="%H" -- bench/ 2>/dev/null || echo "unknown")"

# Warn if the committed baseline is stale (spelunk condition only)
if [[ "$CONDITION" == "spelunk" ]]; then
    if [[ -f "$BASELINE_FILE" ]]; then
        BASELINE_HASH="$(python3 -c "import json; d=json.load(open('${BASELINE_FILE}')); print(d.get('scaffold_hash','unknown'))")"
        if [[ "$BASELINE_HASH" != "$SCAFFOLD_HASH" && "$BASELINE_HASH" != "unknown" ]]; then
            echo "Warning: committed baseline scaffold_hash (${BASELINE_HASH}) does not match"
            echo "         current bench/ HEAD (${SCAFFOLD_HASH})."
            echo "         Consider regenerating: bash $0 --condition baseline"
            echo ""
        fi
    else
        echo "Warning: no committed baseline found at ${BASELINE_FILE}."
        echo "         Run with --condition baseline first, then commit the result."
        echo ""
    fi
fi

API_KEY="${API_KEY:-${DEEPSEEK_API_KEY:-}}"

EXTRA_ARGS=(--api-key "$API_KEY")
if [[ -n "$REPO_PATH" ]]; then
    EXTRA_ARGS+=(--repo-path "$REPO_PATH")
fi

echo "Condition:    ${CONDITION}"
echo "Split:        ${SPLIT}"
echo "Samples:      ${SAMPLES}"
echo "Model:        ${MODEL}"
echo "API base:     ${API_BASE_URL}"
echo "API key:      ${API_KEY:0:8}..." if API_KEY else "(not set)"
echo "Output:       ${OUT_FILE}"
echo ""

uv run --with-requirements "${BENCH_DIR}/requirements.txt" \
    python3 "${SCRIPT_DIR}/evaluate.py" \
    --condition "$CONDITION" \
    --split "$SPLIT" \
    --samples "$SAMPLES" \
    --model "$MODEL" \
    --api-base-url "$API_BASE_URL" \
    --scaffold-hash "$SCAFFOLD_HASH" \
    --out "$OUT_FILE" \
    ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"}

# If spelunk condition and baseline exists, print comparison
if [[ "$CONDITION" == "spelunk" && -f "$BASELINE_FILE" ]]; then
    echo ""
    echo "=== Comparison vs committed baseline ==="
    uv run --with-requirements "${BENCH_DIR}/requirements.txt" \
        python3 "${BENCH_DIR}/report.py" \
        "$BASELINE_FILE" \
        "$OUT_FILE"
fi
