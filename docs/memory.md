# Project Memory

`inkentry memory` is a per-project knowledge store. Use it to capture decisions, context, requirements, questions, and handoff notes that would otherwise live only in chat history or someone's head.

Memory entries are stored in a local SQLite database by default, and (with
`store_in_git_notes` enabled, the default) also written through to
`refs/notes/inkentry` on `HEAD`. Sharing them is a separate, opt-in step:
`git push` does not push `refs/notes/*`, so your entries stay on your machine
until you install the pre-push hook with `inkentry hooks install --pre-push`
(see [Sharing memory across clones via
git-notes](#sharing-memory-across-clones-via-git-notes)). No external
database or server is required. (You can make git-notes the primary backend with
`--backend git-notes`, or point at a shared server with `server_url`.) The auto-started local `inkentry-server` (loopback) is used only for *inference* (embeddings/LLM for semantic search); it does **not** store memory. Memory lives on a server only when you *explicitly* configure a team `server_url` **and** opt into `mode = "cloud_first"`; with the default `local_first` mode the server is a converging replica and reads/writes stay local (see [Team server and sync modes](#team-server-and-sync-modes)). Entries
are searchable by full text at all times; semantic search (by meaning) is
available when a server is running — the local one is autostarted on demand.

To verify that entries are being written to git notes, inspect them by
hand with stock git. They live on the `inkentry` ref, so you must name it: plain
`git notes show HEAD` reads git's default `commits` ref and reports "no note
found" even when inkentry has written entries.

```bash
git notes --ref=inkentry show HEAD    # notes on the current commit
git notes --ref=inkentry list         # every commit carrying inkentry notes
# equivalently
GIT_NOTES_REF=refs/notes/inkentry git notes show HEAD
```

**Carrier and index.** Think of `refs/notes/inkentry` as the durable *carrier*
for memory and `.inkentry/memory.db` as the queryable *index* built over it. Every
`memory add` appends its entry to the carrier through one write-through path;
`inkentry init` hydrates the index by importing those notes: `memory list` and
`context` see them immediately, and `inkentry memory reindex` adds the
embeddings the semantic ranking of `inkentry search` needs. Both live in the repo, and the store of record stays local
unless you configure a team `server_url` with `mode = "cloud_first"` (see [Team
server and sync modes](#team-server-and-sync-modes)). The carrier reaches teammates only once
the notes ref is pushed and fetched (see [Sharing memory across clones via
git-notes](#sharing-memory-across-clones-via-git-notes) below).

**Entry identity.** An entry is identified by what it says, not by where or when
it was recorded. inkentry derives a canonical identity for every entry as a
SHA-256 over exactly its `kind`, `title`, and `body`. Two people who
independently record the same decision in two clones arrive at the same
identity, with no server and no coordination between them. Mutable metadata is
deliberately excluded, so tagging, archiving, or superseding an entry never
changes its identity. `memory list` leads each entry with the first 12
characters of this identity and `memory show` prints it in full as `entity_id`.
(The `id` beside it is the local store's own token for the entry rather than an
identity: each machine mints its own, and an import mints a new one.)

Because identity is content-derived, two clones can carry their own copy of the
same entry, and once the notes refs meet, both copies are there. `inkentry memory
list` and `inkentry context` therefore fold copies by identity as they read, so a
decision two people recorded independently appears once rather than twice. The
surviving entry carries the earliest recording time, and the union of the copies'
`tags` and `linked_files` (values are added, never removed). An entry archived in
any copy reads as archived everywhere, so archiving it on one machine does not
un-archive when a still-active copy arrives from another.

**Archiving travels too.** `memory archive <id>` appends a state-update record
for the entry to the carrier, carrying archived status and invalidation time,
rather than editing the entry's line in place. This holds on both storage
paths: the default SQLite-primary path (a write-through carry, best-effort and
non-fatal like `memory add`) and explicit `--backend git-notes` (git notes is
the primary store, and `archive` itself appends the record there). Because the
write is always an append, a clone that already holds an independent copy of
the pre-archive entry still converges to one archived entry once the notes ref
is fetched and merged, instead of leaving a stale active copy sitting
alongside it.

**Superseding travels too.** `--supersedes <old-id>` (on `memory add`) and
`memory supersede` both append a state-update record for the *old* entry to the
carrier — archived status, invalidation time, and an edge naming the new
entry's identity — rather than editing the old entry's line in place, so the
edge survives a re-`init` renumbering `id` and reaches teammates once the notes
ref is fetched and merged. This works identically before and after `inkentry
init`: a clone that never received the new entry still renders the old one as
archived, just without a name for what replaced it.

Both `--supersedes <old-id>` and `memory supersede` require OLD to currently
be active: superseding an entry that is already archived (for example,
running `--supersedes` a second time against the same OLD) fails with an
error — "No active memory entry with id `<old-id>` (old)." — rather than
silently succeeding, on both storage paths (pre- and post-`init`). This
prevents two different, conflicting supersede records from being written for
the same old entry.

**Before `inkentry init`**, `memory add` and `memory list` still work when you are
inside a git repository: with no `.inkentry/` project, `add` rides the same
write-through carrier (there is no SQLite primary yet) and `list` reads entries
back from `refs/notes/inkentry`. Because it is the same write path pre- and
post-`init`, every note carries an identical record shape. `inkentry search` and
`context` remain gated to projects with `.inkentry/` (they need the index to
search and embed).

**Store priority** (unchanged from [ADR-004](adr/004-unified-memory-storage.md)):

1. Explicit `--db <path>` (always wins)
2. Explicit `--backend git-notes` (git notes is the primary store)
3. Explicit team `server_url` in config with `mode = "cloud_first"` (remote
   server; under the default `local_first` mode a configured `server_url` does
   *not* redirect reads or writes, see [Team server and sync
   modes](#team-server-and-sync-modes))
4. A local `.inkentry/memory.db` (after `inkentry init`)
5. No project but inside a git repo: the git-notes write-through carrier (add/list only)
6. Neither a project nor a git repo: error, *"no inkentry project here, and not inside a git repo. Run 'inkentry init' first, or run inside a git repository."*

**Concurrent `add` commands are serialized.** git-notes writes take a
cross-process lock (one lock file in the git common dir, shared across
worktrees), so simultaneous writers append to the note instead of overwriting
each other. A writer that cannot take the lock in time never writes unlocked,
since an unserialized write can erase a concurrent writer's entry: pre-`init`,
where git notes is the sole store, `memory add` fails with an error telling you
to retry; post-`init` the entry is already in `memory.db`, and `memory add`
warns on stderr that the carry to git notes failed. On the rare filesystem
where the lock file cannot be created at all, the write proceeds unserialized
and warns on stderr. Note also that notes under `refs/notes/inkentry` are
**not** pushed or fetched by default, so pre-`init` entries stay on the machine
that wrote them until the notes ref is pushed (see [Sharing memory across
clones via git-notes](#sharing-memory-across-clones-via-git-notes) below, and
the [git notes](https://git-scm.com/docs/git-notes) documentation).

See [ADR-067](adr/067-fail-closed-no-local-project.md) for the fail-closed design
and [ADR-068](adr/068-zero-setup-onboarding-git-notes-memory-fallback.md) for the
git-notes carrier rationale.

### Team server and sync modes

Configuring a team `server_url` does not, by itself, redirect reads or writes
to the server. The `mode` config field (or the `INKENTRY_MODE` environment
variable) controls how the CLI reconciles the local store and the server:

| `mode` | reads | writes | when the server is unreachable |
|---|---|---|---|
| `offline` | local | local | never contacted, even with `server_url` set |
| `local_first` (default when `server_url` is set) | local | local | everything keeps working; the local store is unaffected |
| `cloud_first` | server | server | commands fail with an error; local data is never silently substituted |

**`local_first`** is the default whenever `server_url` is configured. Reads
and writes stay in the project's local `memory.db`, so every command keeps
working offline and the team server is a converging replica rather than the
store of record. Because reads never block on the network, local results can
be ahead of or behind the server. A write commits locally and returns
immediately, with no network call in the write's own path; it queues in a
local outbox (the same
`memory.db` rows, not a separate table) until a background reconciler drains
it. From an interactive terminal session, the write opportunistically starts
(or reuses) a local `inkentry-server` and hands it the outbox to push; that
same background process also holds a live pull connection to the team server,
so entries recorded elsewhere on the team tend to show up locally without any
explicit step. Non-interactive invocations (CI, scripts, git hooks) never
auto-start a server: the write still commits and stays durably queued, and
drains the next time an interactive session or an explicit trigger runs.

Because of this, **you normally don't need to run `inkentry sync` by hand** in
`local_first` mode; it now mostly matters for non-interactive contexts (a CI
job that wants entries pushed before it exits) or when you want an immediate,
synchronous push/pull rather than waiting on the background reconciler.
`inkentry status` prints the active mode plus, when there's something to
report, a pending-entry count and how long ago the local store last synced
(for example `mode  local_first  ·  2 pending, last synced 4m ago`); only a
project that has never synced shows no extra clause. Once a project has
synced at least once, the clause persists even after the outbox fully
drains, for example `mode  local_first  ·  up to date, last synced 4m ago`.
Use `inkentry sync` (two-way) whenever you want to force a synchronous reconcile
with the server instead of waiting on the background drain; for a one-way
transfer (seeding, CI) reach for the plumbing forms `inkentry plumbing push`
(local → server) and `inkentry plumbing pull` (server → local). All of these
paths also embed into the local `memory.db`: a push embeds what it sends and a
pull embeds what it applies, so an entry stays findable by semantic `search` on
your own machine whether you wrote it or a teammate did (see
[Repair during push and sync](#repair-during-push-and-sync)).

**`cloud_first`** makes the server authoritative: reads and writes go straight
to it, and an unreachable or untrusted server is a hard error naming the cause
(for certificate trust, see `server_ca` / `INKENTRY_SERVER_CA`). The CLI never
falls back to local data in this mode. Configure it in `.inkentry/config.toml`:

```toml
server_url = "https://inkentry.internal.example.com"
project_id = "my-awesome-app"
mode = "cloud_first"
```

`server_url` may point at a self-hosted `inkentry-server` or at the hosted API.
The two expose different memory routes, so inkentry settles which to speak when
the backend opens, by reading the capability list `/v1/health` already
advertises. A peer advertising SSE memory streaming is the hosted API; anything
else, including a probe that times out, is unreachable, or answers without a
capability list, is treated as a self-hosted server. The probe is
unauthenticated and never sends your server key.

`project_id` may be a slug or a UUID against either peer; every memory
command, including `inkentry memory show` and `inkentry memory archive`, resolves
either form the same way.

Against the hosted API, `inkentry harvest`'s duplicate check filters
locally, because that API has no server-side commit filter. It stays correct,
but its cost grows with the size of the project rather than staying an indexed
lookup.

**`offline`** guarantees no server contact at all, even with `server_url`
set. `INKENTRY_NO_SERVER=1` forces it regardless of config.

### Sharing memory across clones via git-notes

Reading and publishing are not symmetric, and it is worth being precise about
which is automatic:

- **Reading teammates' memory is automatic.** `inkentry init` configures the
  `origin` fetch refspec, so their notes arrive on your next `git fetch`, and
  inkentry merges them on its own read paths.
- **Publishing your own memory is opt-in.** Your memory stays local until you
  install the pre-push hook (or push the notes ref by hand).

When you run `inkentry init` inside a git repository with an `origin` remote,
inkentry automatically configures the fetch refspec for `origin` so that
teammates' `refs/notes/inkentry` travels on `git fetch`. The init command prints
the status:

```
Memory:  configured notes fetch refspec on 'origin' (teammates' memory arrives on fetch)
         your memory stays local until you install the pre-push hook: inkentry hooks install --pre-push
         configured notes.rewriteRef (memory survives `git commit --amend` and `git rebase`)
```

The last line is a separate setting, covered in [Surviving history
rewrites](#surviving-history-rewrites) below. It is printed only by the run that
sets it, so a re-run of `init` omits it.

#### Publishing with the pre-push hook

Install the hook once per clone:

```bash
inkentry hooks install --pre-push
```

From then on, every `git push` to a named remote publishes your memory there:
inkentry fetches the remote's notes, merges them into yours (a union, so nothing
is dropped), and pushes `refs/notes/inkentry`. Once it is installed, `init`
reports that in place of the opt-in line:

```
Memory:  notes fetch refspec already configured on 'origin'
         pre-push hook installed: your memory publishes on `git push`
```

**Publishing is tied to `git push` on purpose.** A note attached to a commit you
have not pushed can reach the remote while the commit itself does not, and a
teammate's clone then cannot resolve what the note is attached to, so the entry
is orphaned: it is on the remote, and nobody ever sees it. Pushing is the moment
that reliably coincides with "this code is being shared", which is why the hook
runs there rather than on each `memory add` or on a timer.

**Publishing needs a named remote.** git tells the hook the *name* of the remote
you are pushing to, and inkentry publishes to that name. So a push that spells out
a URL or a path instead of a remote name publishes nothing:

```bash
git push origin main                  # publishes your memory
git push https://github.com/me/x main # pushes the code only
```

The second form has no remote name to resolve, so the hook skips publishing and
lets your code push through: no error, and your memory simply stays where it was.
This is deliberate. Fetching an arbitrary URL's notes would land them on the
tracking ref reserved for `origin`, overwriting what your real remote put there.
Pushing by URL is uncommon, and a later `git push origin` publishes everything
you have recorded in the meantime. If you push by URL routinely, push the notes
ref by hand (see below).

**The hook never blocks your push.** If publishing fails (offline, or the remote
rejects the notes ref) it warns on stderr and exits 0, so your code push lands
regardless. Only a lost race is retried, up to three times: that is a teammate
publishing in the window between the fetch and the push, so re-merging theirs
lets the next attempt through. Every other failure is attempted once, so an
unreachable remote costs you one timeout rather than three. It never
force-pushes: the union already carries both sides, so forcing could only discard
a teammate's memory.

The one thing that does stop your push is inkentry itself being gone. The hook
records the absolute path of the binary that installed it, rather than looking
`inkentry` up on `PATH`, because GUI git clients on macOS take their environment
from launchd rather than your shell profile, and would otherwise publish nothing
while appearing to work. If you move or reinstall inkentry, that path goes stale
and the hook fails loudly; re-run `inkentry hooks install --pre-push` to
re-resolve it. Remove it entirely with `inkentry hooks uninstall`.

The hook is written to whatever directory `git rev-parse --git-path hooks`
reports, so it honors `core.hooksPath` if you have one set (as husky, lefthook,
and the pre-commit framework do) rather than assuming `.git/hooks`. If that
directory turns out to be tracked and shared (a committed `core.hooksPath`
target like `.husky/`), `install` refuses rather than writing there: a silent
write there would commit inkentry's hook for every teammate on clone instead of
just this machine. Install it into that directory by hand in that case, or
point `core.hooksPath` at an untracked location first.

Teammates never receive the hook otherwise: git does not clone `.git/hooks`, so
installing it affects only your own clone.

#### Publishing without the hook

If you would rather not install a hook, push the notes ref yourself:

```bash
git push origin refs/notes/inkentry
```

Re-run this whenever you record memory: each `inkentry memory add` (or remove)
creates a new notes commit that travels only once it is pushed. Push it **after**
you have pushed the commits your entries are attached to, or those entries arrive
orphaned (see above). The hook exists to get that ordering right for you.

The fetch refspec, by contrast, is configured once, so teammates' (and later
clones') `git fetch` then pulls whatever notes you have already pushed.

**How fetched notes become visible.** The refspec fetches into a *tracking* ref,
`refs/notes/origin/inkentry`, rather than over your own `refs/notes/inkentry`.
Fetching straight onto your working ref would force-update it and silently
replace a local note you had not pushed yet. So arrival is **fetch + merge**:
`git fetch` populates the tracking ref, and `inkentry memory list`, `inkentry
context`, and `inkentry init` merge it into `refs/notes/inkentry` (union, no
conflicts, duplicates dropped). That merge is local-only and does no network: it
folds in what your own `git fetch` already brought down, so it works with the
remote unreachable, and it never picks up remote state on its own. Right after a
fetch, `git notes --ref=inkentry` alone will not show a teammate's entry until one
of those inkentry commands has run.

The merge never delays or fails a read. If another inkentry command is writing
notes at that moment, the merge is skipped and the read returns anyway; the union
is idempotent, so the next read folds the entries in.

**For teammates to receive the notes:**

1. Clone the repository normally: `git clone <repo>`
2. Run `inkentry init` in the clone (or manually add the refspec with `git config --add remote.origin.fetch '+refs/notes/inkentry*:refs/notes/origin/inkentry*'`)
3. Fetch: `git fetch`
4. Read: `inkentry memory list` (this is the step that merges the fetched notes in)

A fresh clone does **not** inherit the source's local git config, so `git fetch`
alone won't pull the notes. The teammate must either run `inkentry init` (which
configures the refspec automatically) or add it manually, then fetch.

**If there is no `origin` remote** (for example, in a local-only or detached
repository), `inkentry init` prints the commands to run later:

```
Memory:  no 'origin' remote, so the notes refspec is not configured
         run later: git config --add remote.origin.fetch '+refs/notes/inkentry*:refs/notes/origin/inkentry*'
         your memory stays local until you install the pre-push hook: inkentry hooks install --pre-push
         configured notes.rewriteRef (memory survives `git commit --amend` and `git rebase`)
```

Add the refspec when an `origin` is created, then publish as above. The
`notes.rewriteRef` line appears here too: rewrites are purely local, so that
setting is configured even in a repository with no remote.

If the repository already carries memory on `refs/notes/inkentry` (for example a
fresh clone of a project whose team records memory through git notes), `inkentry
init` **hydrates** the new `memory.db` from those notes: every entry not already
present is imported, and `inkentry memory list` then shows the repo's recorded
history. The import is idempotent (re-running `init` imports nothing) and copies
entry content only, not embeddings, so imported entries appear in `memory list`
and `context` right away; `inkentry search` ranks them once they
are embedded, which you can do with
[`inkentry memory reindex`](#backfilling-missing-embeddings). This is a local import: the notes must already
be present in your clone. Their cross-machine arrival still depends on your git
notes refspec, since git does not fetch `refs/notes/*` by default (see above).
git-notes is the durable carrier here and `memory.db` is a local index rebuilt
from it; see
[ADR-068](adr/068-zero-setup-onboarding-git-notes-memory-fallback.md).

### Surviving history rewrites

`git commit --amend` and `git rebase` replace a commit with a new sha. A note is
bound to the sha it was written on, and git carries it onto the replacement only
when `notes.rewriteRef` names the ref the note lives on. That setting has **no
built-in default**: in an unconfigured repository, amending or rebasing a commit
that carries memory orphans every entry on the dead sha. `memory list` never
shows those entries again, because it lists notes that are reachable from `git
log`, and the dead sha no longer is.

inkentry therefore points `notes.rewriteRef` at `refs/notes/inkentry` for you. It
is written to the repository's own config, never your global config, at whichever
of these comes first:

- `inkentry init`, alongside the fetch refspec. Independent of `origin`, since
  rewrites are purely local, so a repository with no remote is covered too.
- The first `memory add` write-through, which reaches repositories where you
  never run `init`.
- The `--backend git-notes` write path, where notes are the primary store.

Setting it is announced, never silent. The run that sets it prints:

```
Configured git notes.rewriteRef in this repo, so memory now survives `git commit --amend` and `git rebase`.
```

Later runs stay quiet, since the setting is already in place. `--add` composes
with any value you set yourself rather than replacing it, and an existing value
that already covers the ref (exactly, or via a glob that stays inside
`refs/notes/`) is left alone. If the setting cannot be written, inkentry warns and
continues rather than failing the write, and names the command to run:

```
Warning: could not set git notes.rewriteRef, so memory may not survive `git commit --amend` or `git rebase`. Set it with: git config --add notes.rewriteRef refs/notes/inkentry
```

`notes.rewriteMode` is deliberately left at its `concatenate` default, which
keeps both sides when two noted commits are squashed into one. `overwrite` and
`ignore` each drop one of them, causing the loss this is meant to prevent.

**Known limitation:** git honours `notes.rewriteRef` for `commit --amend` and
`rebase` only. `git merge --squash`, and cherry-picking onto a divergent base, do
**not** carry notes, even with the setting configured. Memory attached to a
commit that reaches your branch by either of those routes is still orphaned on
the original sha. If those entries matter, re-record them on the new commit
before the original is discarded.

This is about surviving a rewrite of your own local history. It is a separate
concern from whether notes reach a teammate, which still depends on the notes ref
being pushed and fetched (see above).

## Why memory?

Code tells you *what* the system does. Memory tells you *why* it was built that way.

Examples of things worth storing:

- "We chose sqlite-vec over pgvector because the project must run without a Postgres server."
- "The embedding format is `title: {name} | text: {content}` — changing this invalidates all stored embeddings."
- "Current question: should the harvester dedupe by commit SHA or by entry content hash?"
- "Handoff to next session: the graph migration is done, secrets scanner is next."

## Memory kinds

| Kind | Use for |
|------|---------|
| `decision` | Architecture or design choices with rationale |
| `context` | Background information that helps understand the codebase |
| `requirement` | Product or technical requirements |
| `note` | General observations (default) |
| `question` | Open questions that need an answer |
| `answer` | Answers to previously stored questions |
| `handoff` | State transfer between work sessions or agents |
| `intent` | Active work signal; surfaced by `inkentry context` with file-overlap warnings |
| `antipattern` | Things to avoid; list with `inkentry memory failures` |

## Storing a note

```bash
# Quick note with body inline
inkentry memory add --title "Chunker uses a token-aware sliding window as fallback" \
              --body "Applies to unsupported file types and oversized semantic nodes: whole lines accumulate up to MAX_CHUNK_TOKENS (2048), with ~12.5% overlap between windows." \
              --kind context \
              --tags chunker,indexer

# Open your $EDITOR for the body (omit --body)
inkentry memory add --title "Decision: use blake3 for file hashing" --kind decision

# Link to specific files
inkentry memory add --title "Auth middleware refactored" \
              --body "Moved session validation to src/auth/middleware.rs" \
              --files "src/auth/middleware.rs,src/auth/session.rs"

# Record when a decision became valid (ISO 8601)
inkentry memory add --title "Adopted monorepo layout" --kind decision \
              --valid-at 2026-01-15

# Supersede an old entry — archives it and records a supersedes edge
inkentry memory add --title "New auth approach" --kind decision --body "..." \
              --supersedes <old-id>

# Mark two entries as related — creates a relates_to edge
inkentry memory add --title "Follow-up note" --kind note --body "..." \
              --relates-to <other-id>
```

When `--body` is omitted, `inkentry` opens `$VISUAL` or `$EDITOR` (falling back to `vi`). Lines starting with `#` are stripped (comment convention).

## Pulling in context from a URL

`--from-url` fetches content from a GitHub issue, Linear ticket, or any web page and stores it as a memory entry. The title is inferred from the page automatically.

```bash
# GitHub issue — uses `gh api` for clean structured content
inkentry memory add --from-url https://github.com/owner/repo/issues/42

# Override the inferred title
inkentry memory add --from-url https://github.com/owner/repo/issues/42 \
              --title "Auth: session token storage compliance issue" \
              --kind requirement

# Any URL — fetches page title and strips HTML
inkentry memory add --from-url https://linear.app/myteam/issue/ENG-1234/... \
              --kind context

# Combine with tags
inkentry memory add --from-url https://github.com/owner/repo/issues/99 \
              --tags auth,security --kind requirement
```

For GitHub issues, `inkentry` calls `gh api` to get structured issue data (requires the [GitHub CLI](https://cli.github.com/) and `gh auth login`). For all other URLs it does an HTTP GET and extracts readable text.

### Web-to-Markdown hook (opt-in) {#web-to-md-hook}

For non-GitHub URLs, if a script exists at `~/.config/inkentry/scripts/web-to-md.ts`, `inkentry` runs it under `bun` (`bun ~/.config/inkentry/scripts/web-to-md.ts <url>`) and uses its stdout instead of the built-in HTML-stripping fallback — useful for sites that need JS rendering or custom extraction logic. The script's first line (`# Title`) becomes the entry title; the rest becomes the body.

This is opt-in by design: the script only runs if you've placed it at that exact, inkentry-owned path. Requires [`bun`](https://bun.sh) on `PATH`. If `bun` or the script fails, `inkentry` silently falls back to the built-in HTML extraction. Set `INKENTRY_SCRIPTS_DIR` to look for the script in a different directory instead of `~/.config/inkentry/scripts`.

> **Why this exact path.** The hook is executed on every `--from-url` call, so
> where it lives is a security boundary. A general-purpose location such as
> `~/scripts/` would mean any script planted there — via an unrelated prior
> compromise, or on a shared or managed machine — gets run silently. The
> inkentry-owned directory is a location you manage deliberately for this
> purpose, so nothing lands in it by accident.

## Searching memory

Memory is searched through the unified `inkentry search` command. By default
`search` interleaves code and
memory results into one ranked list — pass `--only-memory` to restrict it to the
memory corpus. `--as-of` and `--expand-graph` are memory-only modifiers.

```bash
# Search memory only — finds entries by meaning
inkentry search "why did we choose sqlite" --only-memory
inkentry search "authentication decisions" --only-memory --limit 5

# Also surface 1-hop relates_to neighbours of each result
inkentry search "authentication decisions" --only-memory --expand-graph

# Full-text over memory only, no embedding or server needed
inkentry search "auth" --only-memory --only-text

# Point-in-time: only entries that were valid at this date
inkentry search "auth decisions" --only-memory --as-of 2026-01-01
```

**Text matching over memory is phrase-exact.** Unlike the code corpus, where
`--only-text` is BM25 over independent terms, the memory matcher quotes the
whole query as a single FTS5 phrase: it matches only entries whose text contains
those words adjacent and in that order. `"auth decisions"` matches; `"decisions
auth"` matches nothing. Semantic ranking has no such constraint, so a
server-backed `search` is the general way to find an entry by wording you did
not use; `memory list` and `context` take no query at all.

## Tracking topic evolution

`inkentry memory timeline` returns all entries related to a topic, sorted by the time they became valid — useful for understanding how a decision or understanding evolved.

```bash
inkentry memory timeline "authentication strategy"
inkentry memory timeline "database choice" --limit 30
inkentry memory timeline "auth" --format json
```

## Listing entries

```bash
# List recent entries (newest first)
inkentry memory list

# Filter by kind
inkentry memory list --kind decision
inkentry memory list --kind question

# More entries
inkentry memory list --limit 50

# Point-in-time snapshot — only entries valid at a given date
inkentry memory list --as-of 2026-01-01

# Filter by commit SHA (exact or prefix)
inkentry memory list --source-ref abc1234
```

`--source-ref <sha>` returns every entry associated with that commit, matched
two ways: harvested entries carry the commit SHA they were harvested from in
their `source_ref` provenance field, and entries added with `inkentry memory add`
are anchored to a commit by the git note that carries them (the same note you
see under `git notes --ref=inkentry show <sha>`). Both are found; the SHA may be
given in full or as a prefix.

`question` and `answer` entries show titles only in list view to avoid context saturation. Use `inkentry memory show <id>` to read the full body.

## Cross-project visibility

When projects are linked with `inkentry link`, `inkentry search`,
`inkentry memory list`, and `inkentry context` automatically surface relevant
memory from linked projects alongside local results. This is how settled
decisions recorded in one project (for example, a Cloud-only architecture
constraint in `cloud-api`) remain visible to agents working in a sibling
project (for example, `inkentry-oss`).

### What crosses project boundaries

Not all memory propagates. Only entries that match **all three** of the
following criteria are surfaced from a linked project:

- **Kind:** `decision` or `requirement` (never `handoff`, `question`, or `note`).
- **Tag:** must carry the tag `locked` (for settled v1 decisions) or
  `cross-project` (for cross-cutting items that are not otherwise locked). Tags
  like `auth` or `database` alone are not sufficient.
- **Status:** `active` only. Archived or superseded cross-project decisions do
  not resurface after they are retracted in the source project.

Decisions and requirements that do not carry `locked` or `cross-project` remain
strictly project-local, regardless of which `inkentry link` edges are configured.

### Source attribution

Every result from a linked project is labelled with its origin so conflicting
decisions between projects are visible and attributable:

- **Text output:** a `[from: <project>]` badge appended to the entry line.
- **JSON output:** `source_project` and `source_project_path` fields on the
  note object (absent on local results, so existing JSON consumers are
  unaffected).

Local results always appear first; cross-project results are appended, in
registry dependency order, after all local results. The existing `--limit` flag
applies only to the local query; cross-project results are additional and not
counted against the limit.

### Skipping the dep pass

Pass `--local-only` to any of `search`, `memory list`, or `context` to
query only the primary project's memory store:

```bash
inkentry search "auth decisions" --only-memory --local-only
inkentry memory list --kind decision --local-only
inkentry context --local-only
```

### Tagging decisions for cross-project visibility

```bash
# Tag a decision as locked so linked projects can see it
inkentry memory add --kind decision \
  --title "SSE memory stream is Cloud-only" \
  --body "OSS inkentry-server must not expose SSE; Cloud API owns that surface." \
  --tags v1,locked

# Tag a requirement that applies across all linked projects
inkentry memory add --kind requirement \
  --title "All writes validated for secrets before storage" \
  --body "Applies to cloud-api and inkentry-oss alike." \
  --tags security,cross-project
```

### Privacy boundary

The dep pass reads each linked project's `memory.db` directly from disk (local
SQLite only). It does not route through `inkentry-server` or any remote endpoint.
A linked project's memory is only reachable if its `memory.db` file is
accessible on the local filesystem (same machine, same user). Remote or
server-backed linked projects whose memory lives exclusively on a remote server
are not queried by the dep pass in v1.

## Showing a single entry

```bash
inkentry memory show 0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33
inkentry memory show 0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33 --format json
```

`memory show` displays the full body plus any incoming and outgoing relationship edges (supersedes, relates_to, contradicts) with linked entry titles.

## Relationship graph

```bash
# Show all edges for an entry (text)
inkentry memory graph 0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33

# Machine-readable
inkentry memory graph 0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33 --format json
```

**Where these edges live.** All three kinds travel with the repository.
`relates_to` and `contradicts` ride `refs/notes/inkentry` on the record of the
entry each edge starts from, naming their target by its portable entry id, and
the graph is rebuilt from them when the notes are imported. `supersedes`
travels as it always has, as supersede state on the entry it archives, so a
clone shows the superseded entry as archived; the edge itself is not currently
rebuilt as a row in the clone's `memory.db`.

An edge is applied only once both entries it joins are present. One naming an
entry that has not arrived (a partial fetch, or a target recorded with
`store_in_git_notes = false`) is skipped rather than failing the import, and
the number skipped is reported; a later import, after the missing entries
arrive, applies it. Two other paths also carry edges off the machine that
recorded them: `inkentry sync` pushes `relates_to` edges to a configured team
server, and a [portable dump](dump-format.md) carries all three kinds into
whichever store imports it.

## Harvesting from git history

`inkentry harvest` reads your git log, sends commit messages to the LLM, and automatically extracts significant entries. Requires a reachable `inkentry-server` with a chat model loaded; there is no local-model path, and setting `llm_model` in `~/.config/inkentry/config.toml` has no effect on this command (see [Config reference](config-reference.md#llm_model)).

```bash
# Default: last 10 commits (fewer if the repo has fewer than 10)
inkentry harvest

# Custom range
inkentry harvest --git-range HEAD~30..HEAD
inkentry harvest --git-range v1.0..HEAD
```

The default range is clamped to the commits that actually exist, so harvest works on a brand-new repo with a single commit — it never fails with a raw `git` "bad revision" error just because the history is shorter than the range.

Already-harvested commits are skipped (tracked via a `git:<sha>` tag). Routine commits ("fix typo", "wip", etc.) are ignored by the LLM.

`--branch` and `--git-range` values (or either endpoint of an `A..B` range) that start with `-` are rejected before reaching `git`, with a clear error — this prevents a malformed or attacker-controlled ref from being parsed as a `git log` option.

### Automatic harvesting

Install the git hook and harvesting happens on every commit:

```bash
inkentry hooks install
```

To also publish memory to your remote on every `git push`, install the pre-push
hook (see [Sharing memory across
clones](#sharing-memory-across-clones-via-git-notes)):

```bash
inkentry hooks install --pre-push
```

## Importing from a local server

`inkentry memory reconcile` imports notes that were recorded by a running
`inkentry-server` daemon into the project's local `memory.db`. This is useful
after a session where entries were written through `server_url` and need to be
pulled into the project's local store, or when migrating from server-backed to
local storage.

Dedup is by content identity (see **Entry identity** at the top of this page): a
note whose `kind`, `title`, and `body` already match an entry in `memory.db` is
skipped. The source `server.db` is opened read-only; it is never modified.

Because that identity covers only those three fields, source rows that differ
*only* in creation time, tags, or linked files are the same entry, and collapse
into a single row on import. Tags and linked files are merged onto the survivor,
adding but never removing, so nothing recorded on a collapsed copy is lost.
`--format json` reports the count as `collapsed_duplicates`, and the counts
partition the source rows exactly:
`candidates == already_present + collapsed_duplicates + imported`.

```bash
# Import notes for the active project (default source: ~/.local/state/inkentry/server.db)
inkentry memory reconcile

# Preview what would be imported without writing anything
inkentry memory reconcile --dry-run

# Import notes for all projects found in server.db
inkentry memory reconcile --all-projects

# Override the source path
inkentry memory reconcile --source-db /var/run/inkentry/server.db

# Machine-readable summary
inkentry memory reconcile --format json
```

Exit codes: `0` on success or when there is nothing to import, non-zero on
hard errors (unreadable source DB, write failure). When `server.db` does not
exist the command is a no-op and exits 0.

If reconcilable notes are detected at startup, inkentry prints a one-time nudge
to stderr. Set `INKENTRY_NO_RECONCILE_NUDGE=1` to suppress it in CI or scripts.

### Security notes

`reconcile` opens `server.db` with `SQLITE_OPEN_READONLY` and `PRAGMA
journal_mode=WAL` to avoid blocking the daemon's writers. No content from
`server.db` is executed or passed to an LLM; the only write target is the
project's own `memory.db`. Embeddings are re-generated from the imported text
via the configured server (best-effort; notes import successfully even when the
server is unreachable).

## Collapsing duplicate entries already in memory.db

Content identity (see **Entry identity** at the top of this page) is derived
only from `kind`, `title`, and `body`. A `memory.db` that predates this, or
that picked up entries from more than one machine, can already contain rows
that share that identity while differing in `created_at`, `tags`,
`linked_files`, or `status`, for example the same decision recorded twice by
a repeated `harvest` run. inkentry leaves those rows in place: they are
harmless, and nothing reads or writes memory any differently because of them.

Because collapsing existing rows means deleting some of them, it never
happens automatically. If `inkentry` finds duplicate groups while opening a
project's `memory.db`, it logs an actionable one-line warning instead of
touching anything:

```
entity_id has 2 duplicate group(s); run `inkentry memory dedupe` to collapse
them, then re-run inkentry to enforce uniqueness
```

`inkentry memory dedupe` is the command that warning points at. It is a
one-time backfill you run when you want to clean up, not a step in the normal
workflow: `init`, `memory add`, and every other command keep working with
duplicates present.

For each group of rows sharing an identity, the surviving row is the one with
the earliest `created_at`. The other rows are deleted after their `tags` and
`linked_files` are merged into the survivor (union, nothing dropped), its
status becomes `archived` if any row in the group was archived, and any
`supersedes` edge elsewhere in the store that pointed at a deleted row is
repointed to the survivor. One run collapses every duplicate group in a
single transaction: an error midway rolls the whole run back, so `memory.db`
either ends up fully collapsed or is left exactly as it was.

```bash
# Preview what would be collapsed, without writing anything
inkentry memory dedupe --dry-run

# Collapse duplicate groups
inkentry memory dedupe

# Machine-readable summary
inkentry memory dedupe --format json
```

Sample output, collapsing one group of two rows (the newer row's `cli`,
`exit-codes` tags and its linked file merge into the survivor):

```
$ inkentry memory dedupe --dry-run
[inkentry] dedupe (dry-run): total_notes=2 duplicate_groups=1 rows_would_collapse=1

$ inkentry memory dedupe
[inkentry] dedupe: total_notes=2 duplicate_groups=1 rows_collapsed=1 tags_merged=2 linked_files_merged=1 supersede_edges_repointed=0 supersede_self_edges_dropped=0

$ inkentry memory dedupe --format json
{"total_notes":2,"duplicate_groups":1,"rows_collapsed":1,"tags_merged":2,"linked_files_merged":1,"supersede_edges_repointed":0,"supersede_self_edges_dropped":0}
```

Once a store has zero duplicate groups, `dedupe` (dry-run or not) reports
all-zero counts and makes no writes, and the next `inkentry` run promotes
`memory.db`'s `entity_id` index to enforce uniqueness, which rules out any
further duplicate group.

Once that index is promoted, a plain `memory add` for byte-identical
`kind`/`title`/`body` content reuses the existing entry rather than inserting a
second row (merging the new call's `tags` and `linked_files` into it,
add-wins) and reports it instead of erroring:

```
$ inkentry memory add --kind decision --title "dup entry" --body "same content"
Stored [decision] #3: dup entry
$ inkentry memory add --kind decision --title "dup entry" --body "same content"
Already recorded as [decision] #3: dup entry
```

### Security notes

`dedupe` only reads and writes the project's own `memory.db`; it never
contacts a server or an LLM. The collapse runs as a single transaction, so a
failure partway through leaves the store exactly as it was rather than
half-collapsed. Deleting the losing rows is destructive and not reversible by
inkentry itself (back up `memory.db`, or your git-notes ref, first if you want
to be able to undo it).

## Backfilling missing embeddings

A note's semantic vector is normally minted when the note is first added
(`inkentry memory add`), and `inkentry plumbing push` / `inkentry sync` mint
one for any entry in the set they are about to push that still lacks it (see
[Repair during push and sync](#repair-during-push-and-sync)). `memory add`
stores the entry first and then waits a few seconds for its vector, so an entry
added while the embedder is busy with a bulk index pass is stored without one
and becomes searchable at the next `inkentry memory reindex` or
`inkentry sync`, whichever comes first. A note that misses every one of those
moments, because no embedder was reachable at all when it was added, or
because it arrived from an `inkentry import` (a portable dump carries no
vectors), stays in `memory.db` **present but unembedded**. Such a note is
still listed by `memory list` and `context`, which take no query. It is
missing from the *semantic* ranking of `inkentry search`, because that ranking
is a KNN over the embedding vectors and this note has none.

Do not count on text search to reach it in the meantime. The memory text matcher
treats the whole query as one contiguous phrase (see
[Searching memory](#searching-memory)), so it finds an unembedded note only when
you happen to type a phrase that appears in it verbatim. `memory reindex` is the
fix, not a text query.

`inkentry memory reindex` is the recovery command: it embeds the notes that have
no vector, using the same embedder `memory add` uses. In the default
`local_first` mode (and `offline`), that is always the local, auto-discovered
loopback `inkentry-server`, even when a team `server_url` is also configured:
inference stays on-machine there regardless of the sync-mode replica setting.
In `cloud_first` with a team `server_url` set, `reindex` is not applicable and
exits with an actionable error, since `memory.db` isn't the store of record in
that mode; `cloud_first` with no `server_url` set behaves like `local_first`
and reindexes against the loopback embedder. Reach for it after an `inkentry
import`, or when notes were added while the embedder was down.

```bash
# Embed every active note that is missing a vector
inkentry memory reindex

# Report how many notes would be embedded, without writing or contacting the embedder
inkentry memory reindex --dry-run

# Re-embed every active note, replacing existing vectors (e.g. after a model or dimension change)
inkentry memory reindex --force

# Also backfill archived notes (default is active notes only)
inkentry memory reindex --include-archived

# Machine-readable summary
inkentry memory reindex --format json
```

Because embedding **is** the point of this command, it needs a reachable
embedder: with none, it prints an actionable error and exits non-zero without
writing anything (unlike `reconcile`, which imports note text even when the
embedder is down). Each note's vector is committed as it completes, so an
interrupted run is resumable: re-running embeds only the notes still missing,
with no duplicate rows, and a run against a fully-embedded store embeds nothing
and exits 0. Progress is printed to stderr; the machine summary (`--format
json`) goes to stdout. Notes are embedded one at a time, so a large backlog
takes a while; `--dry-run` tells you how many are pending first.

`reindex` covers the **memory** store only. The code index has its own re-embed
path (`inkentry index`, which re-embeds changed files' chunks); the two operate
on separate stores and neither substitutes for the other.

When active notes are present but unembedded, the first memory command prints
a one-line notice naming the count and pointing at `inkentry memory reindex`, so
the recall gap is discoverable without `RUST_LOG`. The notice is a pointer, not
an auto-repair: it embeds nothing itself. Running `reindex` is what restores
semantic recall.

### Repair during push and sync

`inkentry plumbing push` and `inkentry sync` embed what they are about to push.
Before the batch is built, every entry in the push set that has no usable local
vector is embedded through the local loopback embedder, using the same document
text and the same document-side embedding call `inkentry memory reindex` uses,
and each vector is committed to `memory.db` as it completes. A pushed entry is
therefore findable by semantic `search` on your own machine afterwards, with no
separate `reindex` step.

This changes what is stored locally, not what is sent. `kind`, `title`, and
`body` are serialised on every push and always were; the vector fields are
additive, so the same entry text travels either way. What a local vector adds is
that a destination advertising `accepts_pushed_vectors` can store the entry
as-is instead of re-embedding it. A destination checks such a vector against
its own embedder before storing it: model tag, `fp32` precision, dimension,
finite components, and an L2 magnitude inside `[0.5, 1.5]`. A vector failing
any of these is refused with `400`, not rescaled. Vectors from the built-in
embedder satisfy all five, so this is invisible to the CLI and matters only to
a client computing vectors some other way.

Scope and limits:

- Both directions are repaired, and they divide the store between them: the
  push repairs active entries not yet synced, and the pull repairs active
  entries that have been, which is where anything arriving from a teammate
  lands. An **archived** entry is in neither pass and still needs
  `inkentry memory reindex --include-archived`, which matters only for
  point-in-time (`--as-of`) semantic queries, since ordinary search does not
  read archived entries.
- A pull repairs by asking which synced entries still lack a vector, rather
  than by remembering what it just applied. An entry that arrives while no
  embedder is reachable is therefore picked up by the next sync or pull, even
  if that run applies nothing at all.
- The embed always runs against the local loopback embedder, never against a
  configured team `server_url`. This is the same way `reindex` resolves its
  embedder.
- Skipped entirely in `cloud_first` mode with a team `server_url` set, where
  `memory.db` is not the store of record. That is the same condition
  `inkentry memory reindex` declines under, so the two commands agree about when
  local embeddings are meaningful.
- With no local embedder reachable, the push still runs to completion and exits
  exactly as it did before, every entry text-only. It prints one warning:

  ```
  warning: 2 entries pushed without a local embedding, so `inkentry search` cannot surface them in this project until `inkentry memory reindex` is run.
  ```

  A single entry's embed failure behaves the same way: that entry goes out
  text-only and is counted in the warning, the rest of the push still gets
  vectors, and the command does not abort.
- The summary line reports how many entries were embedded locally, separately
  from `created` / `skipped` / `failed`:

  ```
  Done. Pushed 4 entries (created 4, skipped 0). Embedded 2 locally.
  ```

  A push that neither minted nor missed a vector prints the summary line
  unchanged.

## Using memory as context

`inkentry search` interleaves memory and code results in one ranked list — memory answers the *why* while the code answers the *how*. Use `--only-memory` when you want just the decisions or `--only-code` for just the code; the default hands your reasoning model both at once for a complete picture.

## Machine-readable output

All memory commands support `--format json`, and setting `AGENT=true` forces JSON mode globally:

```bash
AGENT=true inkentry memory list --kind question
AGENT=true inkentry search "database decisions" --only-memory
```

## Tips

- **Store the "why", not just the "what"** — the code already captures what was built.
- **Use `question` kind actively** — when you hit a decision point you're unsure about, store it. Come back with `inkentry memory list --kind question` at the start of the next session.
- **Use `handoff` kind** at the end of a long session to summarise the current state for your next session (or for another agent).
- **Tag entries** — tags like `auth`, `database`, `performance` make `inkentry memory list` more scannable and improve search relevance.
- **Use `--supersedes` when updating a decision** — it archives the old entry, sets its invalidation time, and creates a traceable edge so you can always follow the chain of reasoning.
- **Use `--relates-to` for non-superseding connections** — linking a follow-up note or a contradicting observation lets `memory graph` and `--expand-graph` surface related context automatically.
- **Use `--as-of` for archaeology** — `inkentry memory list --as-of 2026-01-01` shows the knowledge state at that date, which is useful for post-mortems or understanding old decisions in context.
