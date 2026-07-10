#!/usr/bin/env bash
# Orchestrate SWE-bench agent runs across the pinned 50-task set.
#
# Reads task IDs from bench/agents/tasks_50.json, expects repos to be
# checked out at bench/repos/<task_id>/ (via bench/setup_repos.sh).
#
# This is the canonical batch orchestrator for bench/agents/ (see
# bench/agents/README.md — batch_run.py is a retired duplicate, do not use
# it for new runs).
#
# Usage:
#   bash bench/agents/swebench_run.sh \\
#       --condition baseline \\
#       --harness none \\
#       --model deepseek-v4-flash \\
#       --api-base-url https://api.deepseek.com/v1 \\
#       --api-key "$DEEPSEEK_API_KEY" \\
#       [--tasks 50] [--max-turns 20] [--seed 42] [--eval]
#
# Options:
#   --condition     baseline|spelunk_search|spelunk_full   (required; must be
#                           "baseline" for --harness opencode|claude-code —
#                           those harnesses are generic coding agents, not
#                           spelunk-instrumented, so spelunk_search/spelunk_full
#                           don't apply to them)
#   --harness       none|opencode|claude-code               (default: none)
#                   none:        agent.py's own tool-calling loop (component-clean cell)
#                   opencode:    headless `opencode run`, DeepSeek via a generated
#                                custom-provider opencode.json (native DeepSeek /v1)
#                   claude-code: headless `claude -p`, DeepSeek via its documented
#                                Anthropic-compatible endpoint (or a shim — see
#                                --endpoint-kind)
#   --model         MODEL                                  (default: deepseek-v4-flash)
#   --api-base-url  URL     (default: https://api.deepseek.com/v1; ignored by
#                           --harness claude-code, which has its own
#                           --deepseek-base-url/--endpoint-kind below)
#   --api-key       KEY     (falls back to DEEPSEEK_API_KEY env var)
#   --tasks         N       only run first N tasks (default: 50)
#   --max-turns     N       max agent turns per task (default: 20)
#   --seed          N       random seed (default: 42)
#   --skip-index            skip spelunk index (if repos are pre-indexed;
#                           --harness none only — opencode/claude-code cells
#                           never invoke spelunk, see README "Adapter notes")
#   --repos-dir     DIR     checkout root (default: bench/repos)
#   --patches-dir   DIR     where per-task .patch files are saved
#                           (default: bench/patches/<condition>-<timestamp>)
#   --eval                  automatically run swebench_eval.sh after agent run
#                           (requires Docker and swebench pip package)
#
# --harness claude-code only:
#   --effort        LEVEL   low|medium|high|xhigh|max (default: high) — always
#                           pinned and recorded in provenance
#   --thinking              request step-by-step thinking (recorded in provenance)
#   --endpoint-kind  KIND   anthropic-compat|shim (default: anthropic-compat)
#   --shim-base-url  URL    Anthropic->OpenAI proxy base URL (required with
#                           --endpoint-kind shim)
#   --deepseek-base-url URL override DeepSeek's Anthropic-compat endpoint
#   --no-deepseek           use Claude Code's own ambient Anthropic credentials
#                           instead of DeepSeek (future native Claude-model cells)
#
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
HARNESS="none"
MODEL="deepseek-v4-flash"
API_BASE_URL="https://api.deepseek.com/v1"
API_KEY="${DEEPSEEK_API_KEY:-}"
TASKS=50
MAX_TURNS=20
SEED=42
SKIP_INDEX=false
RUN_EVAL=false
PATCHES_DIR_OVERRIDE=""

# --harness claude-code only
EFFORT="high"
THINKING=false
ENDPOINT_KIND="anthropic-compat"
SHIM_BASE_URL=""
DEEPSEEK_CLAUDE_BASE_URL="https://api.deepseek.com/anthropic"
NO_DEEPSEEK=false

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
        --harness)      HARNESS="$2";                shift 2 ;;
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
        --effort)       EFFORT="$2";                 shift 2 ;;
        --thinking)     THINKING=true;               shift ;;
        --endpoint-kind) ENDPOINT_KIND="$2";         shift 2 ;;
        --shim-base-url) SHIM_BASE_URL="$2";         shift 2 ;;
        --deepseek-base-url) DEEPSEEK_CLAUDE_BASE_URL="$2"; shift 2 ;;
        --no-deepseek)  NO_DEEPSEEK=true;            shift ;;
        -h|--help)      usage ;;
        *) echo "Unknown argument: $1" >&2; usage ;;
    esac
done

if [[ -z "$CONDITION" ]]; then
    echo "Error: --condition is required." >&2; usage
fi

case "$HARNESS" in
    none|opencode|claude-code) ;;
    *) echo "Error: --harness must be one of none|opencode|claude-code (got: ${HARNESS})" >&2; exit 1 ;;
esac

# opencode/claude-code are generic coding agents, not spelunk-instrumented —
# spelunk_search/spelunk_full only mean something for --harness none (they
# vary spelunk tool access, which these two harnesses never have). Enforcing
# this here (rather than just documenting it) keeps the per-task result
# JSON's own "condition" field — which each harness script now sets from
# --condition, see harness_opencode.py/harness_claude_code.py — from ever
# disagreeing with what --condition claimed to request.
if [[ "$HARNESS" != "none" && "$CONDITION" != "baseline" ]]; then
    echo "Error: --condition must be baseline for --harness ${HARNESS} (got: ${CONDITION})." >&2
    exit 1
fi

if [[ "$HARNESS" == "claude-code" && "$ENDPOINT_KIND" == "shim" && -z "$SHIM_BASE_URL" ]]; then
    echo "Error: --shim-base-url is required with --harness claude-code --endpoint-kind shim." >&2
    exit 1
fi

# API key: not required at all for --harness claude-code --no-deepseek
# (uses Claude Code's own ambient Anthropic credentials).
if [[ "$HARNESS" == "claude-code" && "$NO_DEEPSEEK" == "true" ]]; then
    :
else
    API_KEY="${API_KEY:-${DEEPSEEK_API_KEY:-}}"
    if [[ -z "$API_KEY" ]]; then
        echo "Error: No API key. Use --api-key or set DEEPSEEK_API_KEY env var." >&2
        exit 1
    fi
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
echo "Harness:      ${HARNESS}"
echo "Model:        ${MODEL}"
echo "API base:     ${API_BASE_URL}"
echo "Tasks:        ${TOTAL}"
echo "Max turns:    ${MAX_TURNS}"
echo "Seed:         ${SEED}"
echo "Repos dir:    ${REPOS_DIR}"
if [[ "$HARNESS" == "claude-code" ]]; then
    echo "Effort:       ${EFFORT}"
    echo "Thinking:     ${THINKING}"
    echo "Endpoint:     ${ENDPOINT_KIND}"
fi
echo ""

# ---------------------------------------------------------------------------
# Timestamped output + patches directory
# ---------------------------------------------------------------------------
TS=$(date -u +%Y%m%dT%H%M%SZ)
RESULTS_DIR="${SCRIPT_DIR}/../results"
mkdir -p "$RESULTS_DIR"
# Keep harness=none filenames identical to pre-harness-matrix runs (additive
# only — see spec point 2); only stamp the harness into the filename for the
# new opencode/claude-code cells so they don't collide with each other or
# with a "none" run of the same condition/timestamp.
if [[ "$HARNESS" == "none" ]]; then
    RESULTS_FILE="${RESULTS_DIR}/swebench-${CONDITION}-${TS}.json"
else
    RESULTS_FILE="${RESULTS_DIR}/swebench-${CONDITION}-${HARNESS}-${TS}.json"
fi

if [[ -n "$PATCHES_DIR_OVERRIDE" ]]; then
    PATCHES_DIR="$PATCHES_DIR_OVERRIDE"
elif [[ "$HARNESS" == "none" ]]; then
    PATCHES_DIR="${SCRIPT_DIR}/../patches/${CONDITION}-${TS}"
else
    PATCHES_DIR="${SCRIPT_DIR}/../patches/${CONDITION}-${HARNESS}-${TS}"
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

    # spelunk indexing / memory harvest only apply to the harness=none cell —
    # opencode and claude-code are generic coding-agent harnesses, not
    # spelunk-instrumented (see README "Adapter notes"). Gating on $HARNESS
    # keeps this the single place that decides whether spelunk touches the
    # repo, rather than duplicating the condition check per-harness.
    if [[ "$HARNESS" == "none" ]]; then
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
    fi

    # Build the per-harness command. Each harness script takes the same
    # (task_id, repo_path, issue, model, seed, save-patch) core so that only
    # the harness itself varies between cells (bench/AGENTS.md principle #1).
    PATCH_FILE="${PATCHES_DIR}/${TASK_ID}.patch"
    case "$HARNESS" in
        none)
            RUNNER_ARGS=(
                --condition "$CONDITION"
                --task-id "$TASK_ID"
                --repo-path "$TASK_REPO"
                --issue "$ISSUE_FILE"
                --model "$MODEL"
                --api-base-url "$API_BASE_URL"
                --api-key "$API_KEY"
                --max-turns "$MAX_TURNS"
                --seed "$SEED"
                --save-patch "$PATCH_FILE"
            )
            RUNNER_SCRIPT="${SCRIPT_DIR}/agent.py"
            ;;
        opencode)
            RUNNER_ARGS=(
                --condition "$CONDITION"
                --task-id "$TASK_ID"
                --repo-path "$TASK_REPO"
                --issue "$ISSUE_FILE"
                --model "$MODEL"
                --api-base-url "$API_BASE_URL"
                --api-key "$API_KEY"
                --max-turns "$MAX_TURNS"
                --seed "$SEED"
                --save-patch "$PATCH_FILE"
            )
            RUNNER_SCRIPT="${SCRIPT_DIR}/harness_opencode.py"
            ;;
        claude-code)
            RUNNER_ARGS=(
                --condition "$CONDITION"
                --task-id "$TASK_ID"
                --repo-path "$TASK_REPO"
                --issue "$ISSUE_FILE"
                --model "$MODEL"
                --effort "$EFFORT"
                --endpoint-kind "$ENDPOINT_KIND"
                --deepseek-base-url "$DEEPSEEK_CLAUDE_BASE_URL"
                --max-turns "$MAX_TURNS"
                --seed "$SEED"
                --save-patch "$PATCH_FILE"
            )
            if [[ "$NO_DEEPSEEK" == "true" ]]; then
                RUNNER_ARGS+=(--no-deepseek)
            else
                RUNNER_ARGS+=(--api-key "$API_KEY")
            fi
            if [[ "$THINKING" == "true" ]]; then
                RUNNER_ARGS+=(--thinking)
            fi
            if [[ "$ENDPOINT_KIND" == "shim" ]]; then
                RUNNER_ARGS+=(--shim-base-url "$SHIM_BASE_URL")
            fi
            RUNNER_SCRIPT="${SCRIPT_DIR}/harness_claude_code.py"
            ;;
    esac

    echo "  Running agent (harness=${HARNESS})..."
    RAW_OUTPUT=$(uv run --quiet --with-requirements "${SCRIPT_DIR}/../requirements.txt" python3 "${RUNNER_SCRIPT}" "${RUNNER_ARGS[@]}" 2>&1) || {
        echo "  ERROR: agent crashed for ${TASK_ID}"
        ALL_RESULTS+=("{\"task_id\": \"${TASK_ID}\", \"harness\": \"${HARNESS}\", \"error\": true, \"stderr\": $(printf '%s' "${RAW_OUTPUT}" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')}")
        continue
    }

    # The runner's JSON result is always the last line of stdout — earlier
    # lines can include non-fatal warnings (e.g. "Warning: failed to save
    # patch: ..." from harness_common.extract_patch / agent.py's own
    # --save-patch handler), which end up merged into RAW_OUTPUT via 2>&1
    # above. Extracting the last '{'-prefixed line keeps those warnings from
    # corrupting the final results JSON (matches the same convention already
    # used by batch_run.py's "parse the last JSON line" logic).
    RESULT=$(printf '%s\n' "$RAW_OUTPUT" | grep '^{' | tail -1)
    if [[ -z "$RESULT" ]]; then
        echo "  ERROR: no JSON result line from ${RUNNER_SCRIPT} for ${TASK_ID}"
        ALL_RESULTS+=("{\"task_id\": \"${TASK_ID}\", \"harness\": \"${HARNESS}\", \"error\": true, \"stderr\": $(printf '%s' "${RAW_OUTPUT}" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')}")
        continue
    fi

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
