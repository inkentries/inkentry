#!/usr/bin/env bash
# bench/perf_scale.sh — orchestrator for scale-level performance benchmarks
#
# Runs indexing timing, search latency, and optional memory benchmarks
# across multiple labelled repo sizes, aggregating into a single JSON.
#
# Usage:
#   bash bench/perf_scale.sh --repos small:path medium:path large:path
#   bash bench/perf_scale.sh --repos small:ripgrep medium:django large:linux
#
# Options:
#   --repos       NAME:PATH,...   labelled repos (required)
#   --out         FILE            output JSON (default: bench/results/perf-scale-<ts>.json)
#   --search-iters N              iterations per search query (default: 10)
#   --memory-commits N            commits for git_meta_perf.sh (default: 5000)
#   --skip-memory                 skip the memory benchmark
#   --cold                        remove .spelunk/ before indexing (cold-start timing)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SPELUNK="${SPELUNK:-spelunk}"

REPOS=""
OUT_FILE=""
SEARCH_ITERS=10
MEMORY_COMMITS=5000
SKIP_MEMORY=false
COLD=false

usage() {
    grep '^#' "$0" | grep -v '#!/' | sed 's/^# \?//'
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repos)        REPOS="$2";        shift 2 ;;
        --out)          OUT_FILE="$2";      shift 2 ;;
        --search-iters) SEARCH_ITERS="$2";  shift 2 ;;
        --memory-commits) MEMORY_COMMITS="$2"; shift 2 ;;
        --skip-memory)  SKIP_MEMORY=true;   shift ;;
        --cold)         COLD=true;          shift ;;
        -h|--help)      usage ;;
        *) echo "Unknown argument: $1" >&2; usage ;;
    esac
done

if [[ -z "$REPOS" ]]; then
    echo "Error: --repos is required." >&2; usage
fi

if ! command -v "$SPELUNK" &>/dev/null; then
    echo "Error: spelunk not found in PATH." >&2; exit 1
fi

TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT_FILE="${OUT_FILE:-${SCRIPT_DIR}/results/perf-scale-${TIMESTAMP}.json}"
mkdir -p "$(dirname "$OUT_FILE")"

# Host info for reproducibility
HOST_INFO="$(uname -a 2>/dev/null || echo 'unknown')"
CPU_INFO="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2 | xargs || echo 'unknown')"

echo "=== Performance Scale Benchmarks ==="
echo "Repos:         ${REPOS}"
echo "Search iters:  ${SEARCH_ITERS}"
echo "Output:        ${OUT_FILE}"
echo "Host:          ${CPU_INFO}"
echo ""

RESULTS='{"benchmark":"perf_scale","host":"'"${HOST_INFO}"'","cpu":"'"${CPU_INFO}"'","spelunk_version":"'"$("$SPELUNK" --version 2>&1 | head -1)"'","timestamp":"'"$TIMESTAMP"'","repos":{}}'

# ── Parse repos ────────────────────────────────────────────────────────────
IFS=',' read -ra REPO_ENTRIES <<< "$REPOS"
for entry in "${REPO_ENTRIES[@]}"; do
    NAME="${entry%%:*}"
    REPO_DIR="${entry#*:}"

    if [[ ! -d "$REPO_DIR" ]]; then
        echo "WARNING: repo '$NAME' path '$REPO_DIR' does not exist — skipping." >&2
        continue
    fi

    echo "=== Repo: ${NAME} (${REPO_DIR}) ==="

    # Cold-start: remove existing index so we time a full re-index
    INDEX_MODE="warm"
    if [[ "$COLD" == "true" ]]; then
        if [[ -d "${REPO_DIR}/.spelunk" ]]; then
            echo "  Removing existing .spelunk for cold-start..."
            rm -rf "${REPO_DIR}/.spelunk"
        fi
        INDEX_MODE="cold"
    fi

    # Index it — timed
    echo "  Indexing (${INDEX_MODE})..."
    INDEX_START=$(python3 -c "import time; print(time.monotonic())")
    if ! "$SPELUNK" index "$REPO_DIR" >/dev/null 2>&1; then
        echo "    FAILED — skipping repo" >&2
        continue
    fi
    INDEX_ELAPSED=$(python3 -c "import time; print(round(time.monotonic() - $INDEX_START, 2))")
    echo "    done (${INDEX_ELAPSED}s)"

    # Count files and chunks via spelunk status
    STATS=$(cd "$REPO_DIR" && "$SPELUNK" status --format json 2>/dev/null || echo '{"files":0,"chunks":0}')
    FILES=$(echo "$STATS" | python3 -c "import json,sys; print(json.load(sys.stdin).get('file_count',0))")
    CHUNKS=$(echo "$STATS" | python3 -c "import json,sys; print(json.load(sys.stdin).get('chunk_count',0))")
    echo "    ${FILES} files, ${CHUNKS} chunks"

    # Search latency — 2/7/16 word queries
    echo "  Search latency..."
    SEARCH_RESULT=$(python3 -c "
import json, subprocess, time, statistics
queries = [
    ('error handling', '2 words'),
    ('how does authentication and session management work', '7 words'),
    ('find all functions that parse command line arguments and validate user input configuration', '16 words'),
]
mode = 'hybrid'
all_times = []
for q, label in queries:
    run_times = []
    for _ in range(${SEARCH_ITERS}):
        start = time.monotonic()
        subprocess.run(['${SPELUNK}', 'search', q, '--mode', mode, '--limit', '5', '--format', 'json'],
                       cwd='${REPO_DIR}', capture_output=True, timeout=30)
        elapsed = (time.monotonic() - start) * 1000
        run_times.append(elapsed)
    run_times.sort()
    all_times.append({
        'query': q, 'words': label, 'iterations': len(run_times),
        'p50_ms': round(run_times[len(run_times)//2], 1),
        'p95_ms': round(run_times[int(len(run_times)*0.95)], 1),
        'mean_ms': round(statistics.mean(run_times), 1),
    })
print(json.dumps({'search': all_times}))
" 2>/dev/null || echo '{"search":[]}')

    RESULTS=$(python3 -c "
import json
r = json.loads('''$RESULTS''')
r['repos']['${NAME}'] = {
    'path': '${REPO_DIR}',
    'index_mode': '${INDEX_MODE}',
    'files': ${FILES},
    'chunks': ${CHUNKS},
    'index_seconds': ${INDEX_ELAPSED},
    'files_per_second': round(${FILES} / ${INDEX_ELAPSED}, 1) if ${INDEX_ELAPSED} > 0 else 0,
    'chunks_per_second': round(${CHUNKS} / ${INDEX_ELAPSED}, 1) if ${INDEX_ELAPSED} > 0 else 0,
}
r['repos']['${NAME}'].update(${SEARCH_RESULT})
print(json.dumps(r, indent=2))
")
    echo "    Done."
    echo ""
done

# ── Memory benchmark ────────────────────────────────────────────────────────
if [[ "$SKIP_MEMORY" != "true" ]]; then
    echo "=== Memory at scale (${MEMORY_COMMITS} commits) ==="
    if [[ -x "${SCRIPT_DIR}/git_meta_perf.sh" ]]; then
        bash "${SCRIPT_DIR}/git_meta_perf.sh" "$MEMORY_COMMITS" 2>&1 | tail -5 || echo "  (memory benchmark skipped)"
    else
        echo "  git_meta_perf.sh not found"
    fi
    echo ""
fi

# ── Write output ────────────────────────────────────────────────────────────
echo "$RESULTS" > "$OUT_FILE"
echo "Results written to: ${OUT_FILE}"

python3 -c "
import json
with open('${OUT_FILE}') as f:
    d = json.load(f)
print()
print('Summary:')
for name, repo in d.get('repos', {}).items():
    print(f'  {name}: {repo.get(\"files\",\"?\")} files, {repo.get(\"chunks\",\"?\")} chunks, index={repo.get(\"index_seconds\",\"?\")}s')
    for s in repo.get('search', []):
        print(f'    search ({s[\"words\"]}) p50={s[\"p50_ms\"]}ms  p95={s[\"p95_ms\"]}ms')
"
