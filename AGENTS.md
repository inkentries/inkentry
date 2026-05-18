# AGENTS.md — spelunk

Operational findings and coding conventions for agents working on this codebase.
Companion to `CLAUDE.md` (module map, design decisions, common commands).
Benchmark-specific conventions live in `bench/AGENTS.md`.

---

## Branch Hygiene

- **Always branch from `origin/main`**, never from stale local branches.
  `feat/benchmarking-deepseek` was merged long ago; new work starts at `origin/main`.
- **Verify your branch** before every commit: `git branch --show-current`.
  Accidental commits on the wrong branch are the most frequent operational error.
- **Rebase before push** when the target has moved: `git fetch origin main && git rebase origin/main`.
- **Force-push with lease** after rebase: `git push --force-with-lease`.

---

## Dependabot Rust Fixes

When a Cargo dependency bump breaks the build:

1. **Check for new feature flags.** `gix-hash` >=0.23.0 requires explicit
   `sha1` or `sha256` feature. Fix: add `features = ["sha1"]` to `gix` dep.
2. **Check for API changes in transitive deps.** `git-meta-lib` 0.1.10 changed
   `Session::open(gix::Repository)` to `Session::open(impl Into<PathBuf>)`.
3. **Rebuild Cargo.lock** after conflict resolution with
   `cargo update -p <crate>`.
4. **Run `cargo test`** after every dependency change.

---

## GitHub Workflow

- **One PR per issue.** Branch name: `fix/<issue>-<short-desc>`.
- **PR description:** `Closes #N` or `Refs #N` with bullet summary of changes.
- **Dependabot PRs:** commit fixes directly to the dependabot branch.
  If both bumps touch the same line, fix #240 first (smaller change), then
  rebase #241 onto the merged result.
- **After merge:** the branch is deleted automatically. Move to the next issue.

---

## Agent Communications

This project uses an agent communication protocol defined in
`agent-comms/PROTOCOL.md`. Key points:

- **Inbox:** `agent-comms/inbox/implementer.ndjson` — read at session start.
- **GitHub Issues:** all work tracked as issues. Comment "Starting work on #N"
  and "Done — PR #M" on each.
- **spelunk memory:** store decisions, notes, and handoffs via
  `spelunk memory add --kind <kind> --title "..." --body "..."`.
