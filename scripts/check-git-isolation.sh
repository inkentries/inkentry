#!/usr/bin/env bash
# Fails if any test code spawns `git` (`Command::new("git")`) without wiring
# in this repo's git-config isolation fixture first.
#
# Background: the test suite hides the developer's ambient global/system git
# config from every `git` a test process spawns, via `GIT_CONFIG_GLOBAL` /
# `GIT_CONFIG_SYSTEM=/dev/null` set process-wide behind a `std::sync::Once`
# (see `isolate_git_config()` in the locations below). That mechanism is
# sound, but nothing stops a *new* test file from spawning `git` and simply
# forgetting to wire it in: `crates/spelunk-core/tests/worktree_index_resolution.rs`
# did exactly that despite its own doc-comment claiming to be "fully
# hermetic". This script is the structural backstop: it does not make an
# un-isolated `git` spawn a compile error, but it does make it a CI failure.
#
# Scope and heuristic:
#   - `crates/*/tests/*.rs` (excluding `tests/common/` and `tests/fixtures/`):
#     each file is its own integration-test binary, entirely test code, so a
#     whole-file check is sound: if it spawns git anywhere in the file, it
#     must wire in isolation somewhere in the same file.
#   - `crates/*/src/**/*.rs`: only a trailing `#[cfg(test)] mod tests { ... }`
#     block is test code: the rest of the file is production code that
#     legitimately spawns un-isolated git (e.g. the CLI's own `git notes`
#     plumbing). This script checks from the first `#[cfg(test)]` line to
#     EOF, which holds for every file in this repo today (the test module is
#     always the last item) but is a heuristic, not a parser: a file that
#     stops following that convention could produce a wrong verdict either
#     way, so keep the test module last.
#
# "Wired in isolation" means the checked region contains one of:
#   - a definition of `isolate_git_config` (the file itself IS a canonical
#     helper), or
#   - a call to it, however qualified (`isolate_git_config()`,
#     `common::isolate_git_config()`, ...), or
#   - a call to `common::git_command`/`git_command(` (a constructor that
#     bakes the call in), or
#   - `mod common;` or `mod plumbing_helpers;`, importing the shared
#     fixture module, whose exported constructors call isolate_git_config
#     internally.
#
# Known limitation (accepted, not fixable by a grep-based lint): a spawn
# reached through a variable or an argument built elsewhere, e.g.
# `let bin = "git"; Command::new(bin)`, has no literal `"git"` string next to
# the `new(` call and is invisible to this script. Catching that needs real
# dataflow analysis (a clippy lint or similar), which the story this script
# was added for explicitly scoped out ("a grep/xtask lint is enough, no need
# for a custom clippy lint"). Detected instead: the `new("git")` call itself,
# tolerant of whatever type path prefixes `::new` (`Command::new("git")`,
# `std::process::Command::new("git")`, a renamed `use ... as X; X::new("git")`
# import, ...) and of whitespace/line-wrapping between the tokens (e.g.
# rustfmt breaking a long call across lines).
#
# Usage:
#   scripts/check-git-isolation.sh [root-dir]   # default root-dir: repo root
#   scripts/check-git-isolation.sh --self-test  # regression-tests this script
set -euo pipefail

ISOLATION_MARKER='fn isolate_git_config|isolate_git_config\(|git_command\(|mod common;|mod plumbing_helpers;'
# Matches `<AnyPath>::new("git")` regardless of the type-path prefix (so a
# `use std::process::Command as Proc;` alias is still caught), after
# whitespace/newlines between tokens have been collapsed to single spaces
# below. `[A-Za-z_][A-Za-z0-9_]*::new` requires the call to end in `::new`,
# which every `Command`-constructing path does (`Command::new`,
# `process::Command::new`, `Proc::new`, ...). The trailing `,?` allows for
# rustfmt's trailing comma when a single-argument call gets line-wrapped
# (`Command::new(\n    "git",\n)`).
GIT_SPAWN_PATTERN='[A-Za-z_][A-Za-z0-9_]*::new *\( *"git" *,? *\)'

# Prints an error and sets $fail=1 if `$region` spawns git without an
# isolation marker. $1 is the human-readable label for the offending region.
check_region() {
  local label="$1" region="$2"
  # Collapse all whitespace (including newlines) to single spaces so a
  # call split across lines by rustfmt, or with incidental extra spacing,
  # still matches as a single-line pattern below. Isolation markers are
  # checked against the original (uncollapsed) region: they're simple
  # single-token/single-line calls in every case this repo uses, and
  # collapsing would make the label/context in a future error message less
  # useful for no benefit.
  local collapsed
  collapsed="$(tr -s '[:space:]' ' ' <<<"$region")"

  if ! grep -qE "$GIT_SPAWN_PATTERN" <<<"$collapsed"; then
    return 0
  fi
  if grep -qE "$ISOLATION_MARKER" <<<"$region"; then
    return 0
  fi

  echo "ERROR: $label spawns \`git\` via a \`*::new(\"git\")\` call without wiring in git-config isolation" >&2
  echo "  (expected a call to isolate_git_config()/git_command(), or a \`mod common;\`/\`mod plumbing_helpers;\` import of the shared fixture)" >&2
  fail=1
}

run_check() {
  local root="$1"
  fail=0

  # Integration test binaries: the whole file is test code.
  while IFS= read -r -d '' f; do
    check_region "$f" "$(cat "$f")"
  done < <(find "$root/crates" -path '*/tests/*.rs' \
    -not -path '*/tests/common/*' -not -path '*/tests/fixtures/*' \
    -print0 2>/dev/null)

  # In-crate unit tests: only the trailing `#[cfg(test)] mod tests { ... }`
  # block is test code (see header comment for the "to EOF" assumption).
  while IFS= read -r -d '' f; do
    if grep -q '^#\[cfg(test)\]' "$f"; then
      check_region "$f (#[cfg(test)] region)" "$(sed -n '/^#\[cfg(test)\]/,$p' "$f")"
    fi
  done < <(find "$root/crates" -path '*/src/*.rs' -print0 2>/dev/null)

  return "$fail"
}

self_test() {
  tmp="$(mktemp -d)"
  trap 'rm -rf "${tmp:-}"' EXIT

  mkdir -p "$tmp/crates/fake-crate/tests" "$tmp/crates/fake-crate/src"

  # Bad: spawns git, no isolation marker anywhere in the file.
  cat >"$tmp/crates/fake-crate/tests/bad.rs" <<'EOF'
#[test]
fn spawns_git_unisolated() {
    std::process::Command::new("git").arg("status").status().unwrap();
}
EOF

  # Bad: same, but the call is wrapped across lines the way rustfmt would
  # when the surrounding call gets long. A naive single-line literal-string
  # grep for `Command::new("git")` misses this entirely.
  cat >"$tmp/crates/fake-crate/tests/bad_multiline.rs" <<'EOF'
#[test]
fn spawns_git_unisolated_multiline() {
    std::process::Command::new(
        "git",
    )
    .arg("status")
    .status()
    .unwrap();
}
EOF

  # Bad: same, but with incidental extra whitespace inside the parens.
  cat >"$tmp/crates/fake-crate/tests/bad_whitespace.rs" <<'EOF'
#[test]
fn spawns_git_unisolated_whitespace() {
    std::process::Command::new( "git" ).arg("status").status().unwrap();
}
EOF

  # Bad: same, but via a renamed import, so the literal substring
  # `Command::new(` never appears in the file at all.
  cat >"$tmp/crates/fake-crate/tests/bad_alias.rs" <<'EOF'
use std::process::Command as Proc;
#[test]
fn spawns_git_unisolated_alias() {
    Proc::new("git").arg("status").status().unwrap();
}
EOF

  # Good: spawns git, but wires in the shared fixture module first.
  cat >"$tmp/crates/fake-crate/tests/good.rs" <<'EOF'
mod common;

#[test]
fn spawns_git_isolated() {
    common::isolate_git_config();
    std::process::Command::new("git").arg("status").status().unwrap();
}
EOF

  # Unrelated: no git spawn at all, must never be flagged.
  cat >"$tmp/crates/fake-crate/tests/unrelated.rs" <<'EOF'
#[test]
fn does_nothing_with_git() {
    assert_eq!(2 + 2, 4);
}
EOF

  # src/-side: production code spawns git freely above the test module;
  # only the un-isolated spawn *inside* `#[cfg(test)]` must be flagged.
  cat >"$tmp/crates/fake-crate/src/lib.rs" <<'EOF'
pub fn production_git_spawn() {
    std::process::Command::new("git").arg("rev-parse").status().unwrap();
}

#[cfg(test)]
mod tests {
    #[test]
    fn spawns_git_unisolated_in_test_module() {
        std::process::Command::new("git").arg("status").status().unwrap();
    }
}
EOF

  local failures=0

  if run_check "$tmp" 2>/tmp/self_test_out; then
    echo "SELF-TEST FAIL: run_check should have failed on the bad fixtures, but exited 0" >&2
    failures=1
  else
    if ! grep -q 'tests/bad.rs' /tmp/self_test_out; then
      echo "SELF-TEST FAIL: bad.rs was not flagged" >&2
      failures=1
    fi
    if ! grep -q 'tests/bad_multiline.rs' /tmp/self_test_out; then
      echo "SELF-TEST FAIL: bad_multiline.rs (line-wrapped call) was not flagged" >&2
      failures=1
    fi
    if ! grep -q 'tests/bad_whitespace.rs' /tmp/self_test_out; then
      echo "SELF-TEST FAIL: bad_whitespace.rs (extra spacing) was not flagged" >&2
      failures=1
    fi
    if ! grep -q 'tests/bad_alias.rs' /tmp/self_test_out; then
      echo "SELF-TEST FAIL: bad_alias.rs (renamed Command import) was not flagged" >&2
      failures=1
    fi
    if ! grep -q 'src/lib.rs' /tmp/self_test_out; then
      echo "SELF-TEST FAIL: the un-isolated #[cfg(test)] spawn in lib.rs was not flagged" >&2
      failures=1
    fi
    if grep -q 'tests/good.rs' /tmp/self_test_out; then
      echo "SELF-TEST FAIL: good.rs (isolated) was incorrectly flagged" >&2
      failures=1
    fi
    if grep -q 'tests/unrelated.rs' /tmp/self_test_out; then
      echo "SELF-TEST FAIL: unrelated.rs (no git spawn) was incorrectly flagged" >&2
      failures=1
    fi
  fi

  # Now remove the bad fixtures and confirm a clean tree passes.
  rm "$tmp/crates/fake-crate/tests/bad.rs" \
    "$tmp/crates/fake-crate/tests/bad_multiline.rs" \
    "$tmp/crates/fake-crate/tests/bad_whitespace.rs" \
    "$tmp/crates/fake-crate/tests/bad_alias.rs"
  sed -i.bak '/^#\[cfg(test)\]/,$d' "$tmp/crates/fake-crate/src/lib.rs" && rm -f "$tmp/crates/fake-crate/src/lib.rs.bak"
  if ! run_check "$tmp" 2>/tmp/self_test_out2; then
    echo "SELF-TEST FAIL: run_check should pass once the un-isolated spawns are removed" >&2
    cat /tmp/self_test_out2 >&2
    failures=1
  fi

  if [ "$failures" -eq 0 ]; then
    echo "check-git-isolation self-test: OK"
    return 0
  else
    return 1
  fi
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

root="${1:-.}"
if run_check "$root"; then
  echo "check-git-isolation: OK"
  exit 0
else
  echo "" >&2
  echo "See crates/spelunk-core/tests/common/mod.rs::isolate_git_config and crates/spelunk-cli/tests/plumbing_helpers.rs::isolate_git_config." >&2
  exit 1
fi
