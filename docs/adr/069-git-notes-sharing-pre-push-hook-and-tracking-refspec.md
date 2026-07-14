# ADR-069: Share spelunk memory on `git push`, via an opt-in hook and a tracking-ref fetch refspec

**Date:** 2026-07-14
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** resolves the Open question
[ADR-068](068-zero-setup-onboarding-git-notes-memory-fallback.md) deferred to
^126 ("Does 'travels with the repo' require configuring the notes refspec?").
ADR-068 made `refs/notes/spelunk` the durable carrier for pre-`init` memory,
which put the whole "travels with the repo" promise on that ref being visible
across clones. The answer is yes, and the mechanism currently on `main` (#582)
is wrong in a way that loses notes and breaks plain `git fetch`, so this ADR
also **corrects** it. Leaves [ADR-067](067-fail-closed-no-local-project.md)'s
isolation floor and ADR-004's inference-vs-storage split untouched: everything
here is repo-scoped git plumbing, with no new store of record.

## Context

`git notes` under `refs/notes/spelunk` are not pushed, fetched, or cloned by
default. #582 addressed the fetch half by having `spelunk init` configure an
`origin` fetch refspec (`configure_notes_refspec`,
`crates/spelunk-cli/src/cli/cmd/init.rs:263`), and deliberately left the push
half manual. Its own doc comment states the reason, and it is a real constraint
worth keeping:

> Push refspec is deliberately NOT set: any `remote.origin.push` value
> overrides git's default branch push, so a normal `git push` would stop
> pushing the current branch.

So `init` instead prints a hint to run
`git push origin refs/notes/spelunk` after each memory change. A spike against
git 2.55.0 established that both halves of that design are broken: the manual
push hint produces unreachable notes, and the configured fetch refspec both
breaks `git fetch` and silently destroys local notes. The findings below are
observed behaviour, not projections.

## Decision

**Publish notes on `git push` through an opt-in, non-blocking pre-push hook;
merge with `cat_sort_uniq`; fetch into a tracking ref rather than over the live
one; and have spelunk itself merge that tracking ref on its read paths, so
reading a teammate's memory needs no opt-in.**

### D1 – notes sharing is coupled to `git push`, via an opt-in pre-push hook

Not to `memory add`, and not to a timer.

A note attached to a **locally unpushed commit** can reach origin while its
target object does not. Origin then answers `fatal: could not get object info`,
and a fresh clone fetches the notes ref but cannot resolve its target. The
memory is **orphaned**: teammates read notes on *their* HEAD and never see it.
This is not a corner case. It is what the current post-`memory add` push hint
produces whenever a developer records a decision before pushing the commit it
describes, which is the normal order of work.

Push-on-`memory add` and a background timer both fail the same way, because both
fire independently of whether the commits are pushed, and both force network
egress outside any user-initiated sharing action. `git push` is the only moment
that reliably coincides with "this code is being shared," so it is the only
correct trigger. A pre-push hook also sidesteps the `remote.origin.push`
constraint above: the hook pushes the notes ref as a separate invocation and
never touches the branch-push default.

### D2 – `cat_sort_uniq` is the canonical merge strategy for `refs/notes/spelunk`

spelunk appends each entry as a JSON line to HEAD's note (`append_record`,
`crates/spelunk-core/src/storage/git_notes/mod.rs`), which makes a note a
**union set** of records, not a document with an authored shape. The merge
strategy has to match that. Measured on divergent notes:

| Strategy | Result |
|---|---|
| `cat_sort_uniq` | exit 0, clean union, both entries, no duplicates, no conflict markers |
| `union` | keeps both, injects a blank-line artifact |
| `ours` / `theirs` | exit 0, and **silently destroys one side's memory** |
| default (`manual`) | **exit 1**, CONFLICT, leaves `.git/NOTES_MERGE_WORKTREE` and a stuck partial merge |

Only `cat_sort_uniq` matches the union-set semantics, and it is never the
default, so it must be passed **explicitly** on every merge. A 3-developer
round-trip converged to identical notes with zero loss, and stayed converged
across repeated syncs (idempotent).

Two consequences are accepted deliberately:

- **Concurrent edits union rather than conflict.** There is no user
  interaction to resolve, and force-push never enters the picture.
- **Read order is no longer chronological.** `cat_sort_uniq` sorts lines
  lexicographically, so `read_records`
  (`crates/spelunk-core/src/storage/git_notes/mod.rs`) must sort by `created_at`
  on read. It currently returns records in blob line order, which is
  chronological only because appends happen to land in order today.

### D3 – opt-in, non-blocking, best-effort

- **Installed explicitly** (`spelunk hooks install --pre-push`), never silently.
  It reuses the guard pattern already established by the post-commit hook
  (`crates/spelunk-cli/src/cli/cmd/hooks.rs`,
  `crates/spelunk-cli/src/cli/cmd/init.rs`): bail if a non-spelunk pre-push hook
  is present, keep an idempotent marker, and `command -v spelunk` skip so
  teammates without spelunk are unaffected.
- **Never blocks the user's `git push`.** A hook exiting `1` aborts the branch
  push outright, and in the spike origin never received the commit. Memory
  sharing must not be able to cost someone their push. The hook exits `0`
  unconditionally and warns on stderr.
- **A recursion guard is mandatory.** A naive pre-push hook that pushes the
  notes ref recursed **740 levels deep** and stopped only by exhausting the
  process table (`cannot fork() ... Resource temporarily unavailable`). All
  outer pushes failed invisibly while the branch push still reported success.
  The nested push MUST use `--no-verify`, which makes git skip pre-push
  entirely; an env sentinel is belt-and-braces on top.
- **Retry at most 3 times** on non-fast-forward. This is not a guess: under a
  concurrent 3-way race the third developer only succeeded on attempt 3. Never
  force-push.
- **Hook flow:** `fetch`, then `git notes merge -s cat_sort_uniq` from the
  tracking ref, then `push --no-verify`.
- **`spelunk init` must announce the step.** Opt-in only works if it is
  discoverable, so `init`'s summary output must state that sharing memory with
  teammates requires installing the pre-push hook, and name the command. This is
  a requirement on the implementation, not a docs-only note: a user who never
  reads `docs/` must still learn that their memory stays local until they take
  one more step. It replaces the `PUSH_HINT` line (D4), so `init` stops
  advertising the orphan-prone manual push and points at the hook instead.

### D4 – correct the fetch refspec to a tracking ref

Replace #582's `+refs/notes/spelunk:refs/notes/spelunk` with
**`+refs/notes/spelunk*:refs/notes/origin/spelunk*`**
(`FETCH_REFSPEC`, `crates/spelunk-cli/src/cli/cmd/init.rs:264`). Three separate
findings force this, all on git 2.55.0:

- **The non-glob form breaks plain git.** It requires the remote ref to exist.
  With no notes on the remote, which is every repo until someone pushes,
  `git fetch origin` exits **128** and `git pull` exits **1**, both with
  `fatal: couldn't find remote ref refs/notes/spelunk`. `spelunk init` therefore
  breaks the user's normal git workflow. A glob tolerates the missing remote ref
  and fetch exits 0.
- **The leading `+` on a working ref destroys local notes.** It force-updates
  the destination: a local unpushed note was silently replaced by the remote's
  on a plain `git fetch`, reported only as `(forced update)`, and recoverable
  only via reflog. That is data loss of the product's core asset.
- **A glob alone is not enough.** `…spelunk*:…spelunk*` fixes the fetch break
  but still clobbers the local ref. Only the **tracking** destination is safe.

**Consequence, recorded plainly:** fetched notes land in
`refs/notes/origin/spelunk` and are **not** directly visible to
`git notes --ref=spelunk` or `spelunk memory list` until merged. "Travels
automatically on `git fetch`" therefore becomes **fetch + merge**. D5 decides
who performs that merge.

**No migration.** #582 merged but no release was cut from it, so in practice
nobody carries the broken refspec on disk. Anyone who built that revision and
ran `init` is following the tree closely enough to read a CHANGELOG note and fix
their own config. A CHANGELOG entry covers this; the implementation should not
carry migration code for a population that almost certainly does not exist.

The `init` push hint (`PUSH_HINT`, `init.rs:265`) tells users to push notes
after each memory change, which is exactly the orphaning failure in D1. It is
replaced by the hook install hint (D3), not kept as a fallback.

### D5 – spelunk performs the notes merge on its own read paths

D4 leaves fetched notes in `refs/notes/origin/spelunk`, and D1's hook merges
them only for people who **push**. That is not everyone. A teammate who only
fetches or pulls, the common case for reviewing and reading, would never trigger
a merge and would therefore never see anyone else's memory. Closing that gap
with a complementary fetch hook is not possible: **git has no post-fetch hook.**
The documented hook set on git 2.55.0 is applypatch-msg, commit-msg,
fsmonitor-watchman, the p4-* family, post-applypatch, post-checkout,
post-commit, post-index-change, post-merge, post-receive, post-rewrite,
post-update, pre-applypatch, pre-auto-gc, pre-commit, pre-merge-commit,
pre-push, pre-rebase, pre-receive, prepare-commit-msg, proc-receive,
push-to-checkout, reference-transaction, sendemail-validate, and update. Nothing
in it fires after a bare `git fetch`. The two near misses do not work:
`post-merge` fires on `git pull`'s merge but not on `fetch`, and
`reference-transaction` fires on **every** ref transaction, including the notes
merge's own ref writes (a recursion hazard), at the wrong altitude entirely.

**So spelunk does the merge itself, rather than delegating it to git.** On its
own read paths, spelunk merges `refs/notes/origin/spelunk` into
`refs/notes/spelunk` with `-s cat_sort_uniq`: in `spelunk memory list` and
`spelunk context`, and at `spelunk init`, where the git-notes import already
hydrates the index.

Why this rather than a hook or a git config setting:

- **It covers the fetch-only consumer**, the exact population D1 cannot reach.
- **Nothing to install and no git config surgery.** It works for a teammate who
  never runs `spelunk hooks install`.
- **The strategy stays per-invocation.** spelunk passes `-s cat_sort_uniq` on
  the call and never writes the user's `notes.mergeStrategy`, whose default is
  `manual`. Their own `git notes merge` keeps behaving exactly as they
  configured it.
- **Repeated reads converge.** The union merge is idempotent (D2), so a read
  path can run it every time without drift.

**The cost, recorded honestly:** a read command mutates a local ref, which is
normally a thing to avoid. It is acceptable here because the mutation is
local-only and touches no network, and because it is precisely the carrier to
index hydration that ADR-068's model already implies (git notes are the carrier;
the local store is the queryable index over it). The merge can short-circuit
when the tracking ref has not moved since the last read, so the common case
costs a ref comparison and nothing more.

## Non-goals

- **Not** fixing #185, the read-modify-write race *within* a single repo. That
  is orthogonal and still open. `cat_sort_uniq` resolves the *inter-repo* race
  only.
- **Not** reconciling `NoteRecord.id` collisions. `id` is a local SQLite rowid
  and `remote_id` is only set on server sync, so on the local-first path two
  developers both produce `id:1` for different entries. `cat_sort_uniq` dedupes
  whole lines, so nothing is lost, but the carrier can legitimately hold
  colliding ids. **#598 resolves this**, not this ADR: it amends ADR-068 with a
  canonical content-addressed identity (`content_id`, a `sha256` over the
  entry's canonical JSON semantic core, plus a stable `entity_id` across edits
  and supersede), which removes the rowid collision at its source. This ADR
  depends on nothing more than line-level dedupe, so the two land
  independently.
- **Not** reusing the name `spelunk memory push`. That already means "push local
  memory to the team server," and overloading it onto the notes path would
  conflate two different destinations.
- **Not** setting `remote.origin.push`, for the reason #582 recorded and this
  ADR keeps: it would override git's default branch push.
- **Not** making notes sharing on by default. D3 is opt-in.

## Consequences

- **The promise splits cleanly into reading and publishing.** *Reading*
  teammates' memory is automatic for anyone who fetches, because spelunk merges
  the tracking ref on its own read paths (D5). *Publishing* your own memory is
  opt-in and needs the hook (D1, D3). So "travels with the repo" holds
  unconditionally in the direction users notice first, and the docs should
  describe the asymmetry rather than flatten it.
- **`spelunk init` stops breaking `git fetch` / `git pull`.** This is a bug fix
  to shipped behaviour, not only a new feature, and it is the part that should
  land first.
- **Notes read order must be sorted on read.** D2's lexicographic union is a
  behaviour change that `read_records` has to absorb (sort by `created_at`).
  Anything that assumed blob order is chronological needs to stop.
- **Read commands acquire a local write.** `memory list` and `context` now
  merge a ref (D5). No network, no remote effect, but anything assuming these
  commands are pure reads of local state needs to account for it.
- **No migration, one CHANGELOG line.** #582 shipped in no release, so the
  broken refspec has no real population. The change note tells the handful of
  people tracking `main` to fix their config by hand.
- **`init`'s refspec test changes.** `crates/spelunk-cli/tests/init_notes_refspec.rs`
  pins the old value and its round-trip expectations; both move to the tracking
  ref.
- **Revisit if:** the union-by-default model produces notes large or noisy
  enough that lexicographic union becomes a readability problem, or if the D5
  read-path merge shows up as latency on large notes.

## Security implications

- No new trust boundary and no new store of record. Everything here is
  repo-scoped git plumbing over a ref the user's own repo already holds.
- **Network egress is bounded to the user's own `git push`.** This is stronger
  than the alternatives rejected in D1: push-on-`memory add` and a timer would
  both send data at moments the user did not initiate. The hook adds no egress
  the user was not already performing, and to no host other than the remote they
  chose.
- **D5's read-path merge does not fetch.** It merges the tracking ref the user's
  own `git fetch` already populated, so a read command reaches no network. This
  is deliberate: making reads fetch would put egress back on a code path the
  user did not point at a remote.
- The `memory add` secret scan (`contains_secret`, run before any persistence)
  is unchanged and still runs before anything reaches a note, so the hook can
  only ever push already-scanned content.
- The hook never force-pushes (D3), so it cannot destroy remote history. The
  destructive behaviour identified here is #582's `+` on a working ref (D4),
  which this ADR removes.
