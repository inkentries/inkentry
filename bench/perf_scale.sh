#!/usr/bin/env bash
# bench/perf_scale.sh — orchestrator for scale-level performance benchmarks
#
# Runs indexing, search, and memory benchmarks across multiple repo sizes
# and aggregates results into a single JSON file.
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

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SPELUNK="${SPELUNK:-spelunk}"

REPOS=""
OUT_FILE=""
SEARCH_ITERS=10
MEMORY_COMMITS=5000
SKIP_MEMORY=false

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
        -h|--help)      usage ;;
        *) echo "Unknown argument: $1" >&2; usage ;;
    esac
done

if [[ -z "$REPOS" ]]; then
    echo "Error: --repos is required." >&2; usage
fi

TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT_FILE="${OUT_FILE:-${SCRIPT_DIR}/results/perf-scale-${TIMESTAMP}.json}"
mkdir -p "$(dirname "$OUT_FILE")"

echo "=== Performance Scale Benchmarks ==="
echo "Repos:         ${REPOS}"
echo "Search iters:  ${SEARCH_ITERS}"
echo "Output:        ${OUT_FILE}"
echo ""

RESULTS='{"benchmark":"perf_scale","spelunk_version":"'"$("$SPELUNK" --version 2>&1 | head -1)"'","timestamp":"'"$TIMESTAMP"'","repos":{}}'

# ── Parse repos ────────────────────────────────────────────────────────────
IFS=',' read -ra REPO_ENTRIES <<< "$REPOS"
for entry in "${REPO_ENTRIES[@]}"; do
    NAME="${entry%%:*}"
    PATH="${entry#*:}"

    if [[ ! -d "$PATH" ]]; then
        echo "WARNING: repo '$NAME' path '$PATH' does not exist — skipping." >&2
        continue
    fi

    echo "=== Repo: ${NAME} (${PATH}) ==="

    # Index it
    echo "  Indexing..."
    "$SPELUNK" index "$PATH" 2>&1 | tail -1 || true

    # Count files and chunks
    FILES=$(find "$PATH" -type f -not -path '*/.git/*' -not -path '*/node_modules/*' -not -name '*.pyc' 2>/dev/null | wc -l | tr -d ' ')
    CHUNKS=$("$SPELUNK" chunks --format json "$PATH" 2>/dev/null | python3 -c "import json,sys; print(len([l for l in sys.stdin if l.strip()]))" 2>/dev/null || echo "?")

    echo "    ${FILES} files, ${CHUNKS} chunks"

    # Search latency
    echo "  Search latency..."
    SEARCH_RESULT=$(python3 -c "
import json, subprocess, time, statistics
queries = ['error handling', 'parse command line', 'validate configuration input']
mode = 'hybrid'
all_times = []
for q in queries:
    run_times = []
    for _ in range(${SEARCH_ITERS}):
        start = time.monotonic()
        subprocess.run(['${SPELUNK}', 'search', q, '--mode', mode, '--limit', '5', '--format', 'json'],
                       cwd='${PATH}', capture_output=True, timeout=30)
        elapsed = (time.monotonic() - start) * 1000
        run_times.append(elapsed)
    run_times.sort()
    all_times.append({
        'query': q, 'iterations': len(run_times),
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
    'path': '${PATH}',
    'files': ${FILES},
    'chunks': '${CHUNKS}',
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
        bash "${SCRIPT_DIR}/git_meta_perf.sh" "$MEMORY_COMMITS" 2>&1 | tail -5 || echo "  (memory benchmark skipped — requires spelunk build)"
    else
        echo "  git_meta_perf.sh not found"
    fi
    echo ""
fi

# ── Write output ────────────────────────────────────────────────────────────
echo "$RESULTS" > "$OUT_FILE"
echo "Results written to: ${OUT_FILE}"

# Print summary
python3 -c "
import json
with open('${OUT_FILE}') as f:
    d = json.load(f)
print()
print('Summary:')
for name, repo in d.get('repos', {}).items():
    print(f'  {name}: {repo.get(\"files\",\"?\")} files, {repo.get(\"chunks\",\"?\")} chunks')
    for s in repo.get('search', []):
        print(f'    search p50={s[\"p50_ms\"]}ms  p95={s[\"p95_ms\"]}ms  ({s[\"query\"][:40]})')
"
