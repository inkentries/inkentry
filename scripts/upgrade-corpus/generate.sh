#!/usr/bin/env bash
#
# scripts/upgrade-corpus/generate.sh
#
# Build the upgrade corpus ("DB museum"): artifacts written by real, released
# inkentry binaries, kept so that every future build can be tested against what
# users actually have on disk rather than against an old shape reconstructed by
# hand. A synthetic fixture encodes what we believe the old format was; only a
# real one encodes what it is.
#
# Each wing is produced by downloading a pinned release, running it against a
# small sample repository in a throwaway HOME, and copying the resulting
# database or git bundle into tests/fixtures/upgrade-corpus/wings/<wing-id>/.
# Expected row counts and spot-check values are read out of each artifact with
# plain SQL, before any current-build code opens it, and recorded in
# MANIFEST.json. The test suite asserts the current build preserves them.
#
# Adding a wing at each release: append an entry to the `WINGS` table below and
# to `checksums.txt`, then re-run. Existing wings are only rewritten when
# --only names them, so a new release does not churn the old fixtures.
#
# Prerequisites:
#   * gh (authenticated) to download release assets
#   * python3 and sqlite3
#   * git
# No inkentry-server or model download is needed: the pre-1.0 embedding wire is
# served by embed_stub.py. See that file for what is and is not real.
#
# A NOTE ON NAMES. This script runs binaries that were released under the
# project's former name, so it is written in two vocabularies at once and they
# must not be merged. Anything describing where an artifact goes in this tree
# uses the current name. Anything the OLD BINARY reads or writes — the release
# asset filenames, the binary inside the tarball, its environment variables, its
# config directory, its project directory, its git notes ref, and the text it
# records into the fixtures — keeps the old name, because that is what those
# releases actually do. Each such reference is marked below. Sweeping them into
# the current name breaks regeneration in ways that surface much later, as a
# corpus that no longer matches MANIFEST.json.
#
# The CI job that consumes the corpus needs none of this. It reads the
# checked-in fixtures only.
#
# Usage:
#   scripts/upgrade-corpus/generate.sh              # rebuild every wing
#   scripts/upgrade-corpus/generate.sh --only index-v0.9.2-pre-user-version
#   scripts/upgrade-corpus/generate.sh --list

set -euo pipefail

REPO_SLUG="spelunk-cloud/spelunk"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CORPUS_DIR="$REPO_ROOT/crates/inkentry-cli/tests/fixtures/upgrade-corpus"
WINGS_DIR="$CORPUS_DIR/wings"
MANIFEST="$CORPUS_DIR/MANIFEST.json"
CHECKSUMS="$SCRIPT_DIR/checksums.txt"
CACHE_DIR="${INKENTRY_CORPUS_CACHE:-${TMPDIR:-/tmp}/inkentry-upgrade-corpus-cache}"
STUB="$SCRIPT_DIR/embed_stub.py"
STUB_PORT="${INKENTRY_CORPUS_STUB_PORT:-7799}"

# An old binary predates the file secret-store default and would otherwise
# reach the OS keychain and block on an interactive prompt. Old name: this is
# the variable those releases read.
export SPELUNK_SECRET_STORE=file

# Pinned so git-level metadata is not a source of churn between regeneration
# runs. This does not make a wing byte-reproducible: note ids are epoch millis
# and created_at is wall-clock, both captured by the released binary itself and
# outside this script's control. Compare wings by the MANIFEST sha256, never by
# expecting two runs to produce identical bytes.
export GIT_AUTHOR_NAME="inkentry corpus"
export GIT_AUTHOR_EMAIL="corpus@inkentry.invalid"
export GIT_COMMITTER_NAME="$GIT_AUTHOR_NAME"
export GIT_COMMITTER_EMAIL="$GIT_AUTHOR_EMAIL"
export GIT_AUTHOR_DATE="2026-01-01T00:00:00+00:00"
export GIT_COMMITTER_DATE="$GIT_AUTHOR_DATE"

# wing-id | release tag | producer role
#
# One entry, and the reason there is only one is the point. A wing earns its
# place by covering a path a real user's data actually takes, and neither local
# database is such a path: index.db is not carried at all (the user reindexes)
# and memory.db crosses as a portable dump into a store the current binary
# creates. Wings for those covered migrations nothing performs, and they were
# removed with the ladders they were defending.
#
# The notes ref is the exception. It is renamed in place rather than exported,
# so the blobs a migrating user's ref carries arrive at the current reader
# exactly as the old binaries wrote them. This wing spans three writing eras:
# v0.7.1 wrote one JSON object per note and overwrote it on each add; v0.9.3
# wrote JSON lines without entity ids, and wrote each entry twice; v0.9.5 wrote
# the entity-keyed event log. All three must still read.
#
# Adding a wing is not a matter of picking a release. It is answering yes to:
# does a user's database at some shipped version have to survive the move to a
# newer one? Until that is true there is nothing here worth capturing, and
# `a_schema_version_that_advances_past_the_corpus_fails_here` is what asks the
# question at the moment it stops being hypothetical.
WINGS=(
  "git-notes-eras|v0.9.5|git-notes"
)

# Wire shape and dimension the stub must speak for a given release.
stub_profile() {
  case "$1" in
    v0.6*|v0.7*|v0.8*) echo "768 json" ;;
    *) echo "896 f32le" ;;
  esac
}

host_triple() {
  local arch os
  arch="$(uname -m)"
  os="$(uname -s)"
  case "$arch" in
    arm64|aarch64) arch="aarch64" ;;
    x86_64|amd64) arch="x86_64" ;;
    *) die "unsupported CPU architecture: $arch" ;;
  esac
  case "$os" in
    Darwin) echo "${arch}-apple-darwin" ;;
    Linux) echo "${arch}-unknown-linux-gnu" ;;
    *) die "unsupported OS: $os (release assets cover macOS and Linux)" ;;
  esac
}

die() { echo "error: $*" >&2; exit 1; }
log() { echo "==> $*"; }

# ── release binaries ────────────────────────────────────────────────────────

# Download a release tarball into the cache and verify it against the pinned
# checksum. An unpinned asset is a hard stop, not a warning: the corpus is only
# evidence about a real release if the bytes are the ones that release shipped.
fetch_release() {
  local tag="$1" triple asset dest actual expected
  triple="$(host_triple)"
  # Old name: the published filename of an already-shipped release.
  asset="spelunk-${tag}-${triple}.tar.gz"
  dest="$CACHE_DIR/$asset"

  if [[ ! -f "$dest" ]]; then
    mkdir -p "$CACHE_DIR"
    gh release download "$tag" --repo "$REPO_SLUG" --pattern "$asset" \
      --dir "$CACHE_DIR" --clobber \
      || die "could not download $asset from $REPO_SLUG $tag"
  fi

  actual="$(shasum -a 256 "$dest" | awk '{print $1}')"
  expected="$(awk -v a="$asset" '$2 == a {print $1}' "$CHECKSUMS" 2>/dev/null || true)"
  if [[ -z "$expected" ]]; then
    cat >&2 <<EOF
error: $asset is not pinned in $(basename "$CHECKSUMS").

Verify the download came from the real release, then add this line:

  $actual  $asset

EOF
    exit 1
  fi
  [[ "$actual" == "$expected" ]] \
    || die "$asset checksum mismatch: expected $expected, got $actual"

  # Old name: the executable inside an already-shipped tarball.
  local unpacked="$CACHE_DIR/$tag"
  if [[ ! -x "$unpacked/spelunk" ]]; then
    mkdir -p "$unpacked"
    tar xzf "$dest" -C "$unpacked"
  fi
  [[ -x "$unpacked/spelunk" ]] || die "$asset contains no CLI binary"
  echo "$unpacked/spelunk"
}

# ── sample repo ─────────────────────────────────────────────────────────────

# Deliberately tiny. The corpus is checked in, and what matters for a migration
# is the shape of the tables, not how many rows are in them.
make_sample_repo() {
  local dir="$1"
  mkdir -p "$dir/src"
  cat > "$dir/src/lib.rs" <<'EOF'
pub fn parse_manifest(input: &str) -> usize {
    input.lines().filter(|l| !l.is_empty()).count()
}

pub fn render_manifest(count: usize) -> String {
    format!("{count} entries")
}
EOF
  cat > "$dir/README.md" <<'EOF'
# corpus-sample

A tiny project used to produce the inkentry upgrade corpus.
EOF
  git -C "$dir" init -q
  git -C "$dir" add -A
  git -C "$dir" commit -q -m "corpus sample"
}

# ── embedding stub lifecycle ────────────────────────────────────────────────

STUB_PID=""
start_stub() {
  local dim="$1" wire="$2"
  stop_stub
  python3 "$STUB" "$STUB_PORT" "$dim" "$wire" &
  STUB_PID=$!
  for _ in $(seq 1 40); do
    if curl -fsS "http://127.0.0.1:$STUB_PORT/v1/health" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.25
  done
  die "embedding stub did not come up on port $STUB_PORT"
}

stop_stub() {
  if [[ -n "$STUB_PID" ]] && kill -0 "$STUB_PID" 2>/dev/null; then
    kill "$STUB_PID" 2>/dev/null || true
    wait "$STUB_PID" 2>/dev/null || true
  fi
  STUB_PID=""
}
trap stop_stub EXIT

# Run a released binary with its own HOME, config and registry, so nothing on
# the developer's machine is read or written.
#
# `server_url` is only written when the wing needs vectors. Setting it makes the
# binary demand a project_id for any memory write, which the git-notes wing does
# not have and does not need: note records are plain JSON, no embedding involved.
#
# Old names throughout: the config directory and the two overrides are the ones
# the released binaries resolve. Pointing them at the current name would leave
# the old binary reading the real HOME this function exists to isolate it from.
sandbox_env() {
  local home="$1" want_server="${2:-server}"
  mkdir -p "$home/.config/spelunk"
  if [[ "$want_server" == "server" ]]; then
    printf 'server_url = "http://127.0.0.1:%s"\n' "$STUB_PORT" > "$home/.config/spelunk/config.toml"
  else
    : > "$home/.config/spelunk/config.toml"
  fi
  export HOME="$home"
  export SPELUNK_CONFIG_DIR="$home/.config/spelunk"
  export SPELUNK_REGISTRY_DIR="$home/.config/spelunk"
}

# Fold the write-ahead log back into the main file and store the result gzipped.
# Copying a live database would ship a -wal/-shm pair whose contents the test
# would have to reassemble; the checkpoint makes the single file the whole
# story. The gzip is what makes the corpus checkable in at all: these files are
# mostly the vec0 extension's preallocated vector chunk, which is zeros, so they
# compress by roughly a hundred times.
stage_db() {
  local src="$1" dest="$2"
  sqlite3 "$src" "PRAGMA wal_checkpoint(TRUNCATE);" >/dev/null
  gzip -9 -c "$src" > "$dest"
  rm -f "$src-wal" "$src-shm"
}

# ── wing builders ───────────────────────────────────────────────────────────

build_index_wing() {
  local wing_id="$1" tag="$2" work="$3" out="$4"
  local bin dim wire
  bin="$(fetch_release "$tag")"
  read -r dim wire <<<"$(stub_profile "$tag")"
  start_stub "$dim" "$wire"

  local home="$work/home" repo="$work/repo"
  mkdir -p "$home"
  make_sample_repo "$repo"
  ( sandbox_env "$home"; cd "$repo" && "$bin" index . --force --no-summaries >/dev/null )
  stop_stub

  # Old name: the project directory the released binary writes into.
  [[ -f "$repo/.spelunk/index.db" ]] || die "$tag produced no index.db"
  stage_db "$repo/.spelunk/index.db" "$out/index.db.gz"
}

# Add one entry and echo the id the binary assigned it, parsed from the
# "Stored [kind] #<id>: <title>" confirmation line.
add_memory_entry() {
  local bin="$1" kind="$2" title="$3" body="$4" out id
  out="$("$bin" memory add --kind "$kind" --title "$title" --body "$body")"
  id="$(printf '%s\n' "$out" | sed -n 's/.*#\([0-9][0-9]*\).*/\1/p' | head -1)"
  [[ -n "$id" ]] || die "could not read the entry id out of: $out"
  echo "$id"
}

build_memory_wing() {
  local wing_id="$1" tag="$2" work="$3" out="$4"
  local bin dim wire
  bin="$(fetch_release "$tag")"
  read -r dim wire <<<"$(stub_profile "$tag")"
  start_stub "$dim" "$wire"

  local home="$work/home" repo="$work/repo"
  mkdir -p "$home"
  make_sample_repo "$repo"
  (
    sandbox_env "$home"
    cd "$repo"
    "$bin" init >/dev/null 2>&1 || true
    # Entry ids are assigned by the binary (epoch millis on 0.9.x), so they
    # have to be read back off its output rather than assumed to be 1..n.
    local superseded successor spare
    superseded="$(add_memory_entry "$bin" decision \
      "Chunk with tree-sitter named nodes" \
      "Naive line splits cut functions in half; named AST nodes do not.")"
    successor="$(add_memory_entry "$bin" decision \
      "Chunk with tree-sitter and re-window oversized nodes" \
      "Supersedes the earlier rule: an oversized node still needs a window.")"
    add_memory_entry "$bin" requirement \
      "Index must stay usable without a network" \
      "Full-text search and the code graph run with no server." >/dev/null
    spare="$(add_memory_entry "$bin" note \
      "Retired plan for a separate vector store" \
      "Kept for the record; sqlite-vec removed the need for one.")"
    "$bin" memory supersede "$superseded" "$successor" >/dev/null
    "$bin" memory archive "$spare" >/dev/null
  )
  stop_stub

  local db
  db="$(find "$repo" "$home" -name memory.db -print -quit 2>/dev/null || true)"
  [[ -n "$db" ]] || die "$tag produced no memory.db"
  stage_db "$db" "$out/memory.db.gz"
}

build_registry_wing() {
  local wing_id="$1" tag="$2" work="$3" out="$4"
  local bin dim wire
  bin="$(fetch_release "$tag")"
  read -r dim wire <<<"$(stub_profile "$tag")"
  start_stub "$dim" "$wire"

  local home="$work/home" primary="$work/primary" library="$work/library"
  mkdir -p "$home"
  make_sample_repo "$primary"
  make_sample_repo "$library"
  (
    sandbox_env "$home"
    cd "$library" && "$bin" index . --force --no-summaries >/dev/null
    cd "$primary" && "$bin" index . --force --no-summaries >/dev/null
    cd "$primary" && "$bin" link "$library" >/dev/null
  )
  stop_stub

  # Old name, matching sandbox_env above.
  local reg="$home/.config/spelunk/registry.db"
  [[ -f "$reg" ]] || die "$tag produced no registry.db"
  stage_db "$reg" "$out/registry.db.gz"
}

# One repository carrying all three note-writing eras on refs/notes/spelunk.
#
# Each era gets its own commit, which is not a convenience: releases up to and
# including 0.9.2 replace a commit's note blob instead of appending to it, so
# eras sharing one commit would overwrite each other and only the last would
# survive. A long-lived checkout looks exactly like this, old commits carrying
# old-format notes and newer commits carrying newer ones.
#
# The eras, established by running the releases and reading what they wrote:
#   v0.7.1  one JSON record per blob, replaced on every add
#   v0.9.3  append-only JSON lines, no entity_id
#   v0.9.5  append-only JSON lines, entity-keyed
# The two append-only eras get two entries each, so the multi-line shape is
# genuinely present rather than implied.
build_git_notes_wing() {
  local wing_id="$1" tag="$2" work="$3" out="$4"
  local home="$work/home" repo="$work/repo"
  mkdir -p "$home"
  make_sample_repo "$repo"

  local era
  for era in "v0.7.1|single JSON blob era|1" \
             "v0.9.3|JSON lines era without entity ids|2" \
             "v0.9.5|entity keyed event log era|2"; do
    IFS='|' read -r era_tag era_title era_entries <<<"$era"
    local bin
    bin="$(fetch_release "$era_tag")"
    # A fresh commit per era, so this era's writer cannot clobber the last.
    echo "// $era_title" >> "$repo/src/lib.rs"
    git -C "$repo" add -A
    git -C "$repo" commit -q -m "$era_title"
    (
      sandbox_env "$home" no-server
      cd "$repo"
      local n
      for n in $(seq 1 "$era_entries"); do
        "$bin" memory add --backend git-notes --kind decision \
          --title "$era_title $n" \
          --body "Recorded by spelunk $era_tag." >/dev/null
      done
    )
  done

  # Old names: the ref the released binaries write, and the body text they
  # record, both of which MANIFEST.json asserts against verbatim.
  git -C "$repo" bundle create --quiet "$out/notes.bundle" --all refs/notes/spelunk
}

# ── driver ──────────────────────────────────────────────────────────────────

ONLY=""
case "${1:-}" in
  --list)
    printf '%s\n' "${WINGS[@]}" | cut -d'|' -f1
    exit 0
    ;;
  --only)
    ONLY="${2:?--only needs a wing id}"
    ;;
  "") ;;
  *) die "unknown argument: $1 (try --list)" ;;
esac

command -v gh >/dev/null || die "gh is required to download release assets"
command -v python3 >/dev/null || die "python3 is required"
command -v sqlite3 >/dev/null || die "sqlite3 is required"

mkdir -p "$WINGS_DIR"
WORK_ROOT="$(mktemp -d)"
trap 'stop_stub; rm -rf "$WORK_ROOT"' EXIT

BUILT=()
for entry in "${WINGS[@]}"; do
  IFS='|' read -r wing_id tag kind <<<"$entry"
  if [[ -n "$ONLY" && "$ONLY" != "$wing_id" ]]; then
    continue
  fi
  log "building wing $wing_id from $tag"
  work="$WORK_ROOT/$wing_id"
  out="$WINGS_DIR/$wing_id"
  mkdir -p "$work" "$out"
  rm -f "$out"/*

  case "$kind" in
    index) build_index_wing "$wing_id" "$tag" "$work" "$out" ;;
    memory) build_memory_wing "$wing_id" "$tag" "$work" "$out" ;;
    registry) build_registry_wing "$wing_id" "$tag" "$work" "$out" ;;
    git-notes) build_git_notes_wing "$wing_id" "$tag" "$work" "$out" ;;
    *) die "no builder for wing kind $kind" ;;
  esac
  BUILT+=("$wing_id|$tag|$kind")
done

[[ ${#BUILT[@]} -gt 0 ]] || die "no wings matched${ONLY:+ --only $ONLY}"

log "capturing expectations and writing $(basename "$MANIFEST")"
python3 "$SCRIPT_DIR/capture_expect.py" "$WINGS_DIR" "$MANIFEST" "${BUILT[@]}"

log "done. Wings under $WINGS_DIR"
du -sh "$CORPUS_DIR"
