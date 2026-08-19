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

## 0a. `docker` has the same trap, one layer down

**On macOS, `docker` can block on the login keychain in exactly the same way** — and it is easier to
misread, because a slow first pull is a perfectly normal thing for Docker to be doing.

The symptom: the command sits at

```
Unable to find image 'debian:11' locally
```

and stays there indefinitely without transferring a byte. `docker ps -a` shows **nothing**, because
no container was ever created. Underneath it, the credential helper is waiting on a keychain prompt
nobody can dismiss. It never resolves on its own.

This affects anything that pulls an image, including `scripts/release-dry-run.sh`, whose whole
purpose is building inside `debian:11`.

The fix is a throwaway config with no credential store, so anonymous Docker Hub pulls skip the
helper entirely:

```bash
export DOCKER_CONFIG=$(mktemp -d) && printf '{"auths":{}}' > "$DOCKER_CONFIG/config.json"
```

Then run the command as normal. This changes nothing about your real Docker login; it only removes
the helper from the path for that shell.

> **Same 20-second rule as above, with one adjustment: judge it by bytes, not by time.** A genuine
> pull shows visible layer progress within seconds. A pull that has printed `Unable to find image`
> and nothing since is not slow, it is hung — and an emulated build on Apple silicon is slow enough
> afterwards that "it's probably just slow" is a very easy and very expensive thing to believe.

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
INKENTRY_SECRET_STORE=file INKENTRY_CONFIG_DIR=$(mktemp -d) cargo nextest run --no-fail-fast --lib --bins --tests --benches
INKENTRY_SECRET_STORE=file INKENTRY_CONFIG_DIR=$(mktemp -d) cargo test --doc
```

Scope to the crate you touched while iterating (`-p inkentry-cli`), but run the full suite before
the PR.

### `--no-fail-fast`, and read the Summary count against the total

nextest cancels the whole run on the first failure unless you pass `--no-fail-fast`. One
environmentally-broken test therefore stops the suite wherever it happens to land, and everything
after it silently never runs. `cargo test --doc` needs no equivalent flag; libtest already runs
every test before reporting.

**The only tell is a slash in the Summary line.** A cancelled run reports `N/TOTAL tests run`; a
complete one reports `TOTAL tests run` with no slash. The tests that never ran are not counted
anywhere — not as failures, not as `skipped`:

```
Summary [   3.022s] 2/21 tests run: 1 passed, 1 failed, 0 skipped     <- cancelled: 19 never ran
Summary [  30.359s] 21 tests run: 20 passed, 1 failed, 0 skipped      <- complete
```

That `0 skipped` on the cancelled run is not reassurance; it is the count of nextest's own skips,
and it reads identically either way.

This is not hypothetical. A machine with something already listening on `127.0.0.1:4655` makes
`inference_url` resolve to `Local(...)`, which fails
`capability::llm_route::tests::nothing_configured_anywhere_reports_no_llm_not_offline`. Measured on
this suite: the default run cancelled at `597/2286`, leaving 1689 tests unexecuted; the same run
with `--no-fail-fast` completed 2286 with 2285 passing. Both exit non-zero, so exit status alone
does not distinguish "one known failure, everything else green" from "1689 tests whose state you do
not know".

So: check the number before the `/` against the total. A green-looking run you did not count is a
run you did not watch pass.

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

### 5.2 Comments explain WHY, not WHAT

A comment earns its place by carrying something the code cannot: a hidden constraint, an
invariant, a workaround and the reason for it. A comment restating what the next line plainly does
is noise, so delete it. Trim these as a matter of course when you touch a file, not only when
someone flags them.

### 5.3 No doc-comment syntax in tests

```bash
git diff <base>...HEAD -- '*/tests/*' 'tests/*' | grep -nE '^\+\s*(///|//!)'
```

No rustdoc is generated for tests, so `///` and `//!` there are dead weight. Use plain `//`, or
delete and let the test name carry it.

### 5.4 Content assets are not dead code

Never delete a committed image, video or other binary asset because nothing in the code references
it. Retention can be an intentional content, brand or archival decision that a reference grep
cannot see. Fix the stale doc text, leave the asset, and raise the deletion separately.

---

## Report

State each gate and its result. If you claim green, you ran it.
