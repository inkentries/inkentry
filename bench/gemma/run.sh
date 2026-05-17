#!/usr/bin/env bash
# Orchestrate benchmarks (DeepSeek V4 Flash or any OpenAI-compatible API).
#
# Usage:
#   bash bench/gemma/run.sh --suite crosscodeeval --condition spelunk --repo-path /path/to/repo
#   bash bench/gemma/run.sh --suite swebench --condition spelunk_full
#   bash bench/gemma/run.sh --suite all --condition spelunk --repo-path /path/to/repo
#
# Options:
#   --suite        crosscodeeval|swebench|all       (required)
#   --condition    baseline|spelunk|spelunk_full     (required)
#   --repo-path    PATH      path to indexed repo (required for spelunk conditions)
#   --samples      N         CrossCodeEval samples per language (default: 200)
#   --tasks        N         SWE-bench tasks (default: 50)
#   --model        MODEL     (default: deepseek-v4-flash)
#   --api-base-url URL       (default: https://api.deepseek.com/v1)
#   --api-key      KEY       API key (falls back to DEEPSEEK_API_KEY)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
    grep '^#' "$0" | grep -v '#!/' | sed 's/^# \?//'
    exit 1
}

SUITE=""
CONDITION=""
REPO_PATH=""
SAMPLES=200
TASKS=50
MODEL="deepseek-v4-flash"
API_BASE_URL="https://api.deepseek.com/v1"
API_KEY="${DEEPSEEK_API_KEY:-}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --suite)        SUITE="$2";        shift 2 ;;
        --condition)    CONDITION="$2";    shift 2 ;;
        --repo-path)    REPO_PATH="$2";    shift 2 ;;
        --samples)      SAMPLES="$2";      shift 2 ;;
        --tasks)        TASKS="$2";        shift 2 ;;
        --model)        MODEL="$2";        shift 2 ;;
        --api-base-url) API_BASE_URL="$2"; shift 2 ;;
        --api-key)      API_KEY="$2";      shift 2 ;;
        -h|--help)      usage ;;
        *) echo "Unknown argument: $1" >&2; usage ;;
    esac
done

if [[ -z "$SUITE" || -z "$CONDITION" ]]; then
    echo "Error: --suite and --condition are required." >&2; usage
fi

API_KEY="${API_KEY:-${DEEPSEEK_API_KEY:-}}"

COMMON_ARGS=(--condition "$CONDITION" --model "$MODEL" --api-base-url "$API_BASE_URL" --api-key "$API_KEY")
REPO_ARGS=()
if [[ -n "$REPO_PATH" ]]; then
    REPO_ARGS+=(--repo-path "$REPO_PATH")
fi

run_crosscodeeval() {
    echo "=== CrossCodeEval ==="
    bash "${SCRIPT_DIR}/crosscodeeval/run.sh" \
        "${COMMON_ARGS[@]}" \
        --samples "$SAMPLES" \
        "${REPO_ARGS[@]}"
}

run_swebench() {
    echo "=== SWE-bench (unified agent) ==="
    bash "${SCRIPT_DIR}/../agents/swebench_run.sh" \
        "${COMMON_ARGS[@]}" \
        --tasks "$TASKS"
}

case "$SUITE" in
    crosscodeeval)  run_crosscodeeval ;;
    swebench)       run_swebench ;;
    swebench_local) run_swebench ;;
    all)
        run_crosscodeeval
        echo ""
        run_swebench
        ;;
    *) echo "Error: unknown suite '${SUITE}'. Must be crosscodeeval, swebench, or all." >&2; exit 1 ;;
esac
