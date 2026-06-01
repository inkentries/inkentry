#!/usr/bin/env bash
# Orchestrate SWE-bench agent runs across the pinned 50-task set.
#
# Reads task IDs from bench/agents/tasks_50.json, expects repos to be
# checked out at bench/repos/<task_id>/ (via bench/setup_repos.sh).
#
# Usage:
#   bash bench/agents/swebench_run.sh \\
#       --condition baseline \\
#       --model deepseek-v4-flash \\
#       --api-base-url https://api.deepseek.com/v1 \\
#       --api-key "$DEEPSEEK_API_KEY" \\
#       [--tasks 50] [--max-turns 20] [--seed 42] [--eval]
#
# Options:
#   --condition     baseline|spelunk_search|spelunk_full   (required)
#   --model         MODEL                                  (default: deepseek-v4-flash)
#   --api-base-url  URL     (default: https://api.deepseek.com/v1)
#   --api-key       KEY     (falls back to DEEPSEEK_API_KEY env var)
#   --tasks         N       only run first N tasks (default: 50)
#   --max-turns     N       max agent turns per task (default: 20)
#   --seed          N       random seed (default: 42)
#   --skip-index            skip spelunk index (if repos are pre-indexed)
#   --repos-dir     DIR     checkout root (default: bench/repos)
#   --patches-dir   DIR     where per-task .patch files are saved
#                           (default: bench/patches/<condition>-<timestamp>)
#   --eval                  automatically run swebench_eval.sh after agent run
#                           (requires Docker and swebench pip package)
#   -h|--help

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# Load .env.local via python-dotenv if present
if [[ -f "${REPO_ROOT}/.env.local" ]]; then
    eval "$(uv run --with python-dotenv python3 -c "
import os, dotenv
dotenv.load_dotenv('${REPO_ROOT}/.env.local')
for k, v in os.environ.items():
    if k.startswith('DEEPSEEK_'):
        print(f'export {k}=%s' % repr(v))
" 2>/dev/null)"
fi

CONDITION=""
MODEL="deepseek-v4-flash"
API_BASE_URL="https://api.deepseek.com/v1"
API_KEY="${DEEPSEEK_API_KEY:-}"
TASKS=50
MAX_TURNS=20
SEED=42
SKIP_INDEX=false
RUN_EVAL=false
PATCHES_DIR_OVERRIDE=""

# Default to the shared spelunk-bench checkout if it exists
if [[ -d "${HOME}/opensource/spelunk-bench/repos" ]]; then
    REPOS_DIR="${HOME}/opensource/spelunk-bench/repos"
else
    REPOS_DIR="${SCRIPT_DIR}/../repos"
fi

usage() {
    grep '^#' "$0" | grep -v '#!/' | sed 's/^# \?//'
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --condition)    CONDITION="$2";              shift 2 ;;
        --model)        MODEL="$2";                  shift 2 ;;
        --api-base-url) API_BASE_URL="$2";           shift 2 ;;
        --api-key)      API_KEY="$2";                shift 2 ;;
        --tasks)        TASKS="$2";                  shift 2 ;;
        --max-turns)    MAX_TURNS="$2";              shift 2 ;;
        --seed)         SEED="$2";                   shift 2 ;;
        --skip-index)   SKIP_INDEX=true;             shift ;;
        --repos-dir)    REPOS_DIR="$2";              shift 2 ;;
        --patches-dir)  PATCHES_DIR_OVERRIDE="$2";  shift 2 ;;
        --eval)         RUN_EVAL=true;               shift ;;
        -h|--help)      usage ;;
        *) echo "Unknown argument: $1" >&2; usage ;;
    esac
done

if [[ -z "$CONDITION" ]]; then
    echo "Error: --condition is required." >&2; usage
fi

API_KEY="${API_KEY:-${DEEPSEEK_API_KEY:-}}"
if [[ -z "$API_KEY" ]]; then
    echo "Error: No API key. Use --api-key or set DEEPSEEK_API_KEY env var." >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Resolve task list
# ---------------------------------------------------------------------------
TASKS_FILE="${SCRIPT_DIR}/tasks_50.json"
if [[ ! -f "$TASKS_FILE" ]]; then
    echo "Error: tasks file not found: ${TASKS_FILE}" >&2
    exit 1
fi

ALL_TASK_IDS=$(python3 -c "import json; ids = json.load(open('${TASKS_FILE}')); print(' '.join(ids))")
if [[ "$TASKS" != "50" ]]; then
    ALL_TASK_IDS=$(echo "$ALL_TASK_IDS" | tr ' ' '\n' | head -n "$TASKS" | tr '\n' ' ')
fi
TOTAL=$(echo "$ALL_TASK_IDS" | wc -w | tr -d ' ')
echo "Condition:    ${CONDITION}"
echo "Model:        ${MODEL}"
echo "API base:     ${API_BASE_URL}"
echo "Tasks:        ${TOTAL}"
echo "Max turns:    ${MAX_TURNS}"
echo "Seed:         ${SEED}"
echo "Repos dir:    ${REPOS_DIR}"
echo ""

# ---------------------------------------------------------------------------
# Timestamped output + patches directory
# ---------------------------------------------------------------------------
TS=$(date -u +%Y%m%dT%H%M%SZ)
RESULTS_DIR="${SCRIPT_DIR}/../results"
mkdir -p "$RESULTS_DIR"
RESULTS_FILE="${RESULTS_DIR}/swebench-${CONDITION}-${TS}.json"

if [[ -n "$PATCHES_DIR_OVERRIDE" ]]; then
    PATCHES_DIR="$PATCHES_DIR_OVERRIDE"
else
    PATCHES_DIR="${SCRIPT_DIR}/../patches/${CONDITION}-${TS}"
fi
mkdir -p "$PATCHES_DIR"

ALL_RESULTS=()

# ---------------------------------------------------------------------------
# Run each task
# ---------------------------------------------------------------------------
IDX=0
for TASK_ID in $ALL_TASK_IDS; do
    IDX=$((IDX + 1))
    TASK_REPO="${REPOS_DIR}/${TASK_ID}"

    echo "--- [${IDX}/${TOTAL}] ${TASK_ID} ---"

    # Check repo exists
    if [[ ! -d "$TASK_REPO/.git" ]]; then
        echo "  SKIP: repo not found at ${TASK_REPO}"
        ALL_RESULTS+=("{\"task_id\": \"${TASK_ID}\", \"skipped\": true, \"reason\": \"repo not found\"}")
        continue
    fi

    ISSUE_FILE="${TASK_REPO}/ISSUE.txt"
    if [[ ! -f "$ISSUE_FILE" ]]; then
        echo "  SKIP: ISSUE.txt not found"
        ALL_RESULTS+=("{\"task_id\": \"${TASK_ID}\", \"skipped\": true, \"reason\": \"ISSUE.txt missing\"}")
        continue
    fi

    # Index the repo for spelunk conditions (unless --skip-index)
    if [[ "$CONDITION" != "baseline" && "$SKIP_INDEX" != "true" ]]; then
        echo "  Indexing repo..."
        spelunk index "$TASK_REPO" 2>&1 | tail -1 || true
    fi

    # For spelunk_full: attempt memory harvest from git history.
    # Best-effort — single-commit SWE-bench repos have no harvestable history.
    if [[ "$CONDITION" == "spelunk_full" ]]; then
        echo "  Harvesting memory (best-effort)..."
        spelunk memory harvest --git-range HEAD~50..HEAD "$TASK_REPO" 2>&1 | tail -1 || true
    fi

    # Run the agent
    AGENT_ARGS=(
        --condition "$CONDITION"
        --task-id "$TASK_ID"
        --repo-path "$TASK_REPO"
        --issue "$ISSUE_FILE"
        --model "$MODEL"
        --api-base-url "$API_BASE_URL"
        --api-key "$API_KEY"
        --max-turns "$MAX_TURNS"
        --seed "$SEED"
        --save-patch "${PATCHES_DIR}/${TASK_ID}.patch"
    )

    echo "  Running agent..."
    RESULT=$(uv run --quiet --with-requirements "${SCRIPT_DIR}/../requirements.txt" python3 "${SCRIPT_DIR}/agent.py" "${AGENT_ARGS[@]}" 2>&1) || {
        echo "  ERROR: agent crashed for ${TASK_ID}"
        ALL_RESULTS+=("{\"task_id\": \"${TASK_ID}\", \"error\": true, \"stderr\": $(printf '%s' "${RESULT}" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')}")
        continue
    }

    echo "  Result: ${RESULT}"
    ALL_RESULTS+=("$RESULT")

    # Rate-limit safety: pause between tasks
    sleep 1
done

# ---------------------------------------------------------------------------
# Write final JSON
# ---------------------------------------------------------------------------
printf '%s\n' "${ALL_RESULTS[@]}" | python3 -c "
import json, sys
results = [json.loads(line) for line in sys.stdin if line.strip()]
print(json.dumps(results, indent=2))
" > "$RESULTS_FILE"

echo ""
echo "=== Done ==="
echo "Results:  ${RESULTS_FILE}"
echo "Patches:  ${PATCHES_DIR}/"
echo "Total tasks: ${TOTAL}"
echo "Skipped: $(grep -c '"skipped"' "$RESULTS_FILE" || echo 0)"
echo "Errors:  $(grep -c '"error"' "$RESULTS_FILE" || echo 0)"
echo "Ran:     $(grep -c '"turns"' "$RESULTS_FILE" || echo 0)"

# ---------------------------------------------------------------------------
# Optionally run the Docker harness to compute real resolve_rate
# ---------------------------------------------------------------------------
if [[ "$RUN_EVAL" == "true" ]]; then
    echo ""
    echo "=== Running Docker evaluation (--eval) ==="
    bash "${SCRIPT_DIR}/swebench_eval.sh" \
        --results "$RESULTS_FILE" \
        --patches-dir "$PATCHES_DIR"
else
    echo ""
    echo "To compute resolve_rate, run the Docker harness:"
    echo "  bash bench/agents/swebench_eval.sh \\"
    echo "      --results ${RESULTS_FILE} \\"
    echo "      --patches-dir ${PATCHES_DIR}"
    echo ""
    echo "(Or rerun with --eval to do this automatically.)"
fi
