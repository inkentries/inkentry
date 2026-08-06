---
name: verify
description: Verification gates for the inkentry repo: run before opening or updating any pull request. Covers the mandatory secret-store env var for cargo, formatting, clippy, tests, doctests, and the source-hygiene checks CI and review will otherwise catch. Use after making any change to this repository.
---

# Verify: inkentry

Run every gate below and fix until green. A gate you did not watch pass has not passed.

---

## 0. Every cargo command needs `INKENTRY_SECRET_STORE=file`

**Unconditional. Every cargo invocation, every time. `test`, `build`, `run`, `check`, `clippy`,
`fmt`, `nextest`.** Not a test-only concern: any command that links the crate can reach the
platform secret store.

```bash
INKENTRY_SECRET_STORE=file cargo test -p inkentry-cli
```

Without it, cargo reaches the real OS keyring and **blocks on a live interactive permission
prompt** on the developer's machine. It is not a test failure; the command simply hangs forever
waiting on a dialog someone has to physically dismiss.

Do not rely on inherited environment, a shell export from earlier in the session, or a
`.cargo/config.toml` default. A long-lived or stacked branch may be based on a commit that predates
the config fix, so "it's set in the repo now" is not something you can assume from the branch
you're on. Put it in the command.

> **If any cargo command runs longer than ~20s with no output, kill it immediately** and check for
> this before retrying. Waiting it out costs the whole session; a hung keyring prompt never
> resolves on its own.

## 1. Format

```bash
INKENTRY_SECRET_STORE=file cargo fmt --all -- --check
```

## 2. Clippy: zero warnings

`--lib --bins --tests --benches`, not `--all-targets`: examples are never part
of the regular build/lint/test gates (several depend on the native embedder
and are meant to be run explicitly with the right features, not swept in by a
workspace-wide command that doesn't grant them).

```bash
INKENTRY_SECRET_STORE=file cargo clippy --lib --bins --tests --benches --features rich-formats -- -D warnings
```

## 3. Build

```bash
INKENTRY_SECRET_STORE=file cargo build --lib --bins --tests --benches --features rich-formats
```

## 4. Tests + doctests

```bash
INKENTRY_SECRET_STORE=file INKENTRY_CONFIG_DIR=$(mktemp -d) cargo nextest run --lib --bins --tests --benches
INKENTRY_SECRET_STORE=file INKENTRY_CONFIG_DIR=$(mktemp -d) cargo test --doc
```

Scope to the crate you touched while iterating (`-p inkentry-cli`), but run the full suite before
the PR.

### Isolate the suite from your own inkentry config

`INKENTRY_CONFIG_DIR` overrides the whole config directory, so a fresh temp dir gives the suite the
default configuration instead of yours.

A spawned `inkentry` escapes your own config by one of two routes: the helper that spawns it pins
`INKENTRY_CONFIG_DIR` itself (`plumbing_helpers::inkentry_bin_in`, and the handful of files that build
their own `Command`), or you export `INKENTRY_CONFIG_DIR` for the whole run as above and the child
inherits that. With neither, the child reads `~/.config/inkentry/config.toml`.
So if you have configured inkentry for your own use, particularly a `server_url` with
`mode = "cloud_first"`, a run without the export picks that up and starts talking to a real server.
That has already interfered with real runs: tests that should be hermetic fail, hang, or pass for
the wrong reason depending on whether that server happens to be healthy.

Your own `inkentry` usage and the repo's test runs are different concerns and must not share
configuration. Pointing the suite at a throwaway directory is the cheapest way to keep them apart,
and it matches what CI already gets by having no user config at all.

## 5. Git isolation lint

```bash
scripts/check-git-isolation.sh
```

---

## Source hygiene: check your own diff

Everything below is checked against `git diff <base>...HEAD`, not the whole repo. You are
responsible for what your change introduced.

### 5.1 No external tracker references in shipped text

**This is a public repository.** Never write a reference to an internal tracker, planning board or
ticket id into shipped code, comments, test names, commit messages, docs or ADR text.

```bash
git diff <base>...HEAD | grep -nE '^\+.*(\^[0-9]+|#[0-9]+)'
```

Review every hit. A bare `#123` or `^123` in this repo reads to any future reader as *this repo's*
issue #123, which is a real, unrelated issue. That makes each occurrence a permanently wrong
pointer, not merely internal noise.

Describe **what** changed and **why**: the invariant, the bug, the behaviour. Never **which
ticket** prompted it. Cross-references to real GitHub issues *in this repo* are fine; that is what
the grep is for: reading the hits, not deleting them blindly.

### 5.2 Keep internal conventions out of public text

**This is a public repository, and a PR body, a commit message and a branch name are all public
text.** Never name an internal style convention in any of them, never justify one, and never claim
to have complied with one. Do not name a branch after one either.

Note that this file is committed, so the rule applies to it: the conventions themselves are not
restated here, and should not be added.

Stating that a convention was followed is worse than breaking it. It tells a reader the convention
exists and that we police it, which makes every future artifact readable as the output of that
policing. The convention then signals the exact thing it was adopted to avoid, and no amount of
later compliance undoes it.

Describe **what changed and why**: the behaviour, the bug, the invariant. If a change exists only to
satisfy an internal convention, say what it did to the text, not which rule prompted it.

### 5.3 Comments explain WHY, not WHAT

A comment earns its place by carrying something the code cannot: a hidden constraint, an
invariant, a workaround and the reason for it. A comment restating what the next line plainly does
is noise, so delete it. Trim these as a matter of course when you touch a file, not only when
someone flags them.

### 5.4 No doc-comment syntax in tests

```bash
git diff <base>...HEAD -- '*/tests/*' 'tests/*' | grep -nE '^\+\s*(///|//!)'
```

No rustdoc is generated for tests, so `///` and `//!` there are dead weight. Use plain `//`, or
delete and let the test name carry it.

### 5.5 Content assets are not dead code

Never delete a committed image, video or other binary asset because nothing in the code references
it. Retention can be an intentional content, brand or archival decision that a reference grep
cannot see. Fix the stale doc text, leave the asset, and raise the deletion separately.

---

## Report

State each gate and its result. If you claim green, you ran it.
