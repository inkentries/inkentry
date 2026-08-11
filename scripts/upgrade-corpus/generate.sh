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
# Which vocabulary a release speaks is a property OF THAT RELEASE, not of this
# script: v0.9.8 is the first release built under the current name, and it is
# published from a different repository under different asset names, ships a
# differently-named binary, and reads INKENTRY_* rather than SPELUNK_*.
# `release_repo` and `release_name` below are the single place that boundary is
# encoded; every site that touches a released binary asks them rather than
# hardcoding either name.
#
# The CI job that consumes the corpus needs none of this. It reads the
# checked-in fixtures only.
#
# Usage:
#   scripts/upgrade-corpus/generate.sh              # rebuild every wing
#   scripts/upgrade-corpus/generate.sh --only index-v0.9.2-pre-user-version
#   scripts/upgrade-corpus/generate.sh --list

set -euo pipefail

LEGACY_REPO_SLUG="spelunk-cloud/spelunk"
CURRENT_REPO_SLUG="inkentries/inkentry"
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
# reach the OS keychain and block on an interactive prompt. Both spellings are
# exported because the releases captured here straddle the rename and each one
# reads only its own.
export SPELUNK_SECRET_STORE=file
export INKENTRY_SECRET_STORE=file

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
# The tags are not arbitrary. v0.9.2 is the last release before index.db grew
# PRAGMA user_version, so it is the only way to capture a field DB whose
# version has to be inferred from its table shapes. v0.8.3 is the last release
# that wrote FLOAT[768] vectors. v0.9.3 is the last before memory entries grew
# a content-addressed entity_id. v0.7.1 wrote one JSON object per note and
# overwrote it on each add, the era before the ref became an append-only log.
#
# The list has to reach the STAMPED era too, and for four releases it did not:
# every wing above was captured at user_version 0, so nothing here had ever
# opened a store whose header carries a version at all — a hole that a defect
# walked straight through, since the stamped path is the one every user is on.
# What a stamp decides differs by store. index.db trusts the header, skips
# `infer_legacy_version`, and runs only the steps above it. memory.db no longer
# migrates at all, and the stamp decides which of the two refusals the user is
# handed. The boundaries in that era:
#
#   v0.9.4  last release writing index.db at user_version 14, before `files`
#           grew an mtime column; two steps below the current build, so the
#           runner has to resume mid-ladder from a header it trusts.
#   v0.9.8  newest release, index.db at 15 — the shape sitting in almost every
#           project directory in use today.
#   v0.9.6  the one and only release writing memory.db at user_version 9, the
#           last before entries grew import state.
#   v0.9.8  memory.db at 10, the highest stamp any release ever wrote and so the
#           store most users are holding. This build stamps 11 and refuses
#           everything at or below 10, which makes this the wing that shows the
#           refusal firing where it will be met most often — and naming the
#           version it found, rather than telling the user to upgrade to a build
#           that cannot exist.
#
# v0.9.7 gets no wing: it stamps 15/10, exactly what v0.9.8 already provides for
# both stores. This is a list of boundaries, not a list of releases.
WINGS=(
  "index-v0.8.3-float768|v0.8.3|index"
  "index-v0.9.2-pre-user-version|v0.9.2|index"
  "index-v0.9.4-pre-file-mtime|v0.9.4|index"
  "index-v0.9.8|v0.9.8|index"
  "memory-v0.9.3-pre-entity-id|v0.9.3|memory"
  "memory-v0.9.5|v0.9.5|memory"
  "memory-v0.9.6-pre-import-state|v0.9.6|memory"
  "memory-v0.9.8|v0.9.8|memory"
  "registry-v0.9.5|v0.9.5|registry"
  "git-notes-eras|v0.9.5|git-notes"
)

# Which repository published a release, and the name that release spells
# itself with. See A NOTE ON NAMES above: v0.9.8 is where this flips.
release_repo() {
  case "$1" in
    v0.9.8) echo "$CURRENT_REPO_SLUG" ;;
    *) echo "$LEGACY_REPO_SLUG" ;;
  esac
}

release_name() {
  case "$1" in
    v0.9.8) echo "inkentry" ;;
    *) echo "spelunk" ;;
  esac
}

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
  local tag="$1" triple asset dest actual expected name slug
  triple="$(host_triple)"
  name="$(release_name "$tag")"
  slug="$(release_repo "$tag")"
  # The published filename of an already-shipped release, spelled the way that
  # release spelled itself.
  asset="${name}-${tag}-${triple}.tar.gz"
  dest="$CACHE_DIR/$asset"

  if [[ ! -f "$dest" ]]; then
    mkdir -p "$CACHE_DIR"
    gh release download "$tag" --repo "$slug" --pattern "$asset" \
      --dir "$CACHE_DIR" --clobber \
      || die "could not download $asset from $slug $tag"
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

  # The executable inside an already-shipped tarball carries that release's
  # own name, which is not necessarily the current one.
  local unpacked="$CACHE_DIR/$tag"
  if [[ ! -x "$unpacked/$name" ]]; then
    mkdir -p "$unpacked"
    tar xzf "$dest" -C "$unpacked"
  fi
  [[ -x "$unpacked/$name" ]] || die "$asset contains no CLI binary named $name"
  echo "$unpacked/$name"
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
# The config directory and the two overrides are spelled with the CAPTURING
# RELEASE's name, which is why `name` is a parameter rather than a constant:
# spelling them any other way leaves that binary reading the real HOME this
# function exists to isolate it from.
sandbox_env() {
  local home="$1" name="$2" want_server="${3:-server}" prefix
  prefix="$(printf '%s' "$name" | tr '[:lower:]' '[:upper:]')"
  mkdir -p "$home/.config/$name"
  if [[ "$want_server" == "server" ]]; then
    printf 'server_url = "http://127.0.0.1:%s"\n' "$STUB_PORT" > "$home/.config/$name/config.toml"
  else
    : > "$home/.config/$name/config.toml"
  fi
  export HOME="$home"
  export "${prefix}_CONFIG_DIR=$home/.config/$name"
  export "${prefix}_REGISTRY_DIR=$home/.config/$name"
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
  local bin dim wire name
  bin="$(fetch_release "$tag")"
  name="$(release_name "$tag")"
  read -r dim wire <<<"$(stub_profile "$tag")"
  start_stub "$dim" "$wire"

  local home="$work/home" repo="$work/repo"
  mkdir -p "$home"
  make_sample_repo "$repo"
  ( sandbox_env "$home" "$name"; cd "$repo" && "$bin" index . --force --no-summaries >/dev/null )
  stop_stub

  # The project directory the released binary writes into, under its own name.
  [[ -f "$repo/.$name/index.db" ]] || die "$tag produced no index.db"
  stage_db "$repo/.$name/index.db" "$out/index.db.gz"
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
  local bin dim wire name
  bin="$(fetch_release "$tag")"
  name="$(release_name "$tag")"
  read -r dim wire <<<"$(stub_profile "$tag")"
  start_stub "$dim" "$wire"

  local home="$work/home" repo="$work/repo"
  mkdir -p "$home"
  make_sample_repo "$repo"
  (
    sandbox_env "$home" "$name"
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
  local bin dim wire name
  bin="$(fetch_release "$tag")"
  name="$(release_name "$tag")"
  read -r dim wire <<<"$(stub_profile "$tag")"
  start_stub "$dim" "$wire"

  local home="$work/home" primary="$work/primary" library="$work/library"
  mkdir -p "$home"
  make_sample_repo "$primary"
  make_sample_repo "$library"
  (
    sandbox_env "$home" "$name"
    cd "$library" && "$bin" index . --force --no-summaries >/dev/null
    cd "$primary" && "$bin" index . --force --no-summaries >/dev/null
    cd "$primary" && "$bin" link "$library" >/dev/null
  )
  stop_stub

  # Matching sandbox_env above, under the capturing release's own name.
  local reg="$home/.config/$name/registry.db"
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
    local bin era_name
    bin="$(fetch_release "$era_tag")"
    era_name="$(release_name "$era_tag")"
    # A fresh commit per era, so this era's writer cannot clobber the last.
    echo "// $era_title" >> "$repo/src/lib.rs"
    git -C "$repo" add -A
    git -C "$repo" commit -q -m "$era_title"
    (
      sandbox_env "$home" "$era_name" no-server
      cd "$repo"
      local n
      for n in $(seq 1 "$era_entries"); do
        "$bin" memory add --backend git-notes --kind decision \
          --title "$era_title $n" \
          --body "Recorded by spelunk $era_tag." >/dev/null
      done
    )
  done

  # Bundle whichever notes ref the era binaries actually wrote rather than a
  # hardcoded one: today all three eras pre-date the rename and write
  # refs/notes/spelunk, and a post-rename era added later would write another
  # ref and otherwise be silently left out of the bundle. The ref name and the
  # body text the old binaries record are both asserted verbatim by
  # MANIFEST.json, so neither may be modernised.
  local note_refs=() ref
  while IFS= read -r ref; do
    note_refs+=("$ref")
  done < <(git -C "$repo" for-each-ref --format='%(refname)' refs/notes/)
  [[ ${#note_refs[@]} -gt 0 ]] || die "the era binaries wrote no refs/notes/* ref"
  git -C "$repo" bundle create --quiet "$out/notes.bundle" --all "${note_refs[@]}"
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
