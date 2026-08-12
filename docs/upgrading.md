# Upgrading to 1.0.0

1.0.0 is the first release that will not open a store an earlier build wrote.
Neither store migrates, and the two answer differently:

| Store | What 1.0.0 does | Getting back to working |
|---|---|---|
| `.inkentry/index.db` | Discards it and rebuilds it empty, carrying only the `usage` table | `inkentry index .`. It is derived from your source tree, so nothing is lost. |
| `.inkentry/memory.db` | Refuses to open it, and leaves the file byte for byte as it was | Export the store to a [portable dump](dump-format.md), then `inkentry import` it. |

Memory is authored, so nothing regenerates it. The good news is that the refusal
is not destructive: your store is still there, and the export route stays open
after the fact. The bad news is that the refusal looks like a broken install,
and the obvious way to make it go away, deleting the store and re-running
`init`, is the one action that turns a recoverable situation into a permanent
loss.

So: **get your memory out before you upgrade.** Read
[Before anyone upgrades](#before-anyone-upgrades-get-your-memory-out) for what
"out" has to mean, and
[If you upgraded without exporting](#if-you-upgraded-without-exporting) if you
are already reading this from the far side.

## Why this is a repository event and not a personal one

The instinct is to upgrade when it suits you and let colleagues follow when it
suits them. That works for a tool whose state is entirely yours. It does not
work here, because some of what inkentry writes is shared through the same git
repository as the code.

| Surface | Shared? | Who it affects |
|---|---|---|
| `.inkentry/config.toml` | **Tracked and committed.** `init` writes it and prints a reminder to commit it, so the project slug travels with the repo. | Everyone, on their next pull. |
| `refs/notes/inkentry` | **Pushed and fetched.** The git-notes carrier for memory entries. | Everyone who fetches. |
| Committed scripts, CI steps, agent instructions and hook wrappers that call `inkentry` | **Tracked.** | Everyone, on their next pull. |
| `.inkentry/index.db`, `.inkentry/memory.db` | Private. `.inkentry/.gitignore` lists `index.db*` and `memory.db*`. | You only. |

So the destructive half of this upgrade (the two stores) is per-machine, and
each person pays it once. The half that lands on everybody at once is the
tracked half: the moment one person commits a script or a CI step written
against 1.0.0's output, a colleague still on the previous release pulls it and
runs a command whose contract has changed underneath them.

That is the coordination problem. It is not that the databases spread. It is
that the *instructions for using them* spread, at git's speed, not yours.

There is a second one for teams running a shared `inkentry-server`. See
[Teams running a shared server](#teams-running-a-shared-server).

## Before anyone upgrades: get your memory out

This is a precondition, not a recommendation. Not because the window closes,
the store stays readable to an export tool afterwards, but because doing it
first is the difference between a planned step and a surprise. Everything below
is easier to reason about while your memory is still reachable from ordinary
commands.

Take stock first, with the version you are still running:

```bash
inkentry memory list                 # how much is in the local store
inkentry status                      # where this project's memory actually lives
```

If `status` reports memory living on a server rather than in the local store,
your entries are on that server and this section is about the server's upgrade,
not yours. See [Teams running a shared server](#teams-running-a-shared-server).

### The complete path: a portable dump

A [portable dump](dump-format.md) is line-delimited JSON expressed as entities
and relationships. It carries every memory entry plus the `supersedes`,
`relates_to` and `contradicts` relationships between them, and it is the only
form that carries all three. `inkentry import` reads it into a store 1.0.0
created:

```bash
inkentry import project.dump
```

The import verifies the file whole before writing anything: record counts and
digest are both recomputed, and any mismatch refuses the entire file. It is
also idempotent, which matters for a team. Running the same dump twice reports
nothing new rather than duplicating:

```
Imported 6 memory entries, 0 relationships, 0 projects.
Carried 6 entries into git notes, so they travel with the repository.
```

```
Imported 0 memory entries, 0 relationships, 0 projects.
6 were already in this store and were not added again.
6 were already in this repository's git notes and were not written again.
```

Note the second line of the first run. **An import publishes.** The entries it
landed are appended to `refs/notes/inkentry`, so they reach every colleague who
fetches. That is usually what you want, and it is why one person importing on
behalf of a shared history is a reasonable plan rather than a race. It is also
why you should know it is happening.

**This CLI reads dumps but does not write them.** There is no `inkentry export`.
Writing one is done by a separate export tool, because it has to open a store
this build refuses to open, and it is distributed on its own for that reason.
It takes the store path and the destination, opens the store read-only, and
verifies what it wrote against what it read before reporting success. Keep the
dump somewhere outside `.inkentry/`, which is gitignored for the databases and
is not a backup location.

This is also what the refusal message means by "the migration tool". If you are
reading this because you hit that message, the store is still intact and the
[recovery section](#if-you-upgraded-without-exporting) applies.

### The path with no extra tooling, and exactly what it costs

If your memory has been published to the shared git-notes ref, a fresh 1.0.0
`inkentry init` imports it back out of the ref into the new store it creates,
and says so:

```
  Memory:  imported 6 entries from git notes
```

That makes the ref a usable fallback, with three losses worth naming before you
rely on it:

- **Publishing is opt-in.** Reading a teammate's notes is automatic; publishing
  your own is not. Until you have run `inkentry hooks install --pre-push` (or
  pushed the notes ref by hand), your memory has never left your machine and the
  ref holds nothing of yours. Check before assuming.
- **The carrier cannot represent `relates_to` or `contradicts` edges at all.**
  Entries survive; those two kinds of link do not. `supersedes` does travel.
- **Entries recorded with `store_in_git_notes = false` were never written to the
  carrier**, so they are not there either.

So before you decide the ref is enough:

```bash
inkentry hooks install --pre-push    # if it is not already installed
git push origin <your-branch>        # publishes refs/notes/inkentry
```

Whatever route you take, a plain copy of the file costs nothing and is the
cheapest insurance available:

```bash
cp .inkentry/memory.db ~/memory-backup.db
```

1.0.0 will not open that copy either, but it keeps the option of exporting it
later instead of closing it off.

## A sequence for a team

Most of this is waiting rather than working, and the only step with a real
duration attached is the re-index, which scales with the repository.

**1. Agree a window, and say what it is for.** Half the value of announcing it
is that people stop assuming the upgrade is optional and personal. State the
date, and state the one thing everybody does before installing anything:
export their own memory, from the version they are still running.

**2. One person goes first.** They upgrade, re-index, and import their dump.
Their import lands on `refs/notes/inkentry`, so pushing afterwards puts the
team's accumulated entries onto the shared carrier in a form the new build
reads. They then fix any committed script, CI step or agent instruction that
depended on a changed output shape, and hold that commit until step 4, so
nobody still on the old binary pulls tooling written against the new one.

**3. They report back what actually happened**, including how long the
re-index took on the real repository. That number is what everyone else is
planning around, and it is the one thing a general upgrade note cannot tell
them.

**4. Everyone else upgrades inside the window**, then re-indexes. Their own
import is optional at this point and usually lands nothing new, because the
entries are already on the notes ref. Their own dump, taken back at step 1, is
not optional: it is the only thing carrying their `relates_to` and
`contradicts` edges, which the carrier cannot represent. Once the team is
across, the commit held back at step 2 lands.

**5. Whoever was away.** Someone on holiday pulls two weeks later, still on the
previous release, into a repository whose committed scripts now assume 1.0.0.
Nothing of theirs is destroyed by this: their stores are their own, and the
notes ref stays readable in both directions throughout. What they hit is
tooling that no longer behaves. Their sequence is the same one, just later:
export first, then upgrade, then re-index. The step they skip is the export,
because by then the upgrade feels like catching up rather than a migration,
which is the single reason to write it down in the announcement rather than
assume it.

## Symptoms, if you meet this unprepared

**Memory: a refusal that names the fix.** Every command that reads the local
memory store stops with this, and exits `1`:

```
Error: this memory store was written by an older product (schema version 10) and cannot be opened in place. Export it with the migration tool, then run `inkentry import` to bring it across.
```

The store is not deleted, emptied or rewritten. It is left byte for byte as it
was, which is what keeps the export route open after the fact.

**Search: silence, not an error.** The index rebuild is not a refusal, so
nothing stops. `inkentry search` reports no results and exits `0`:

```
No results found.
```

`inkentry status` is where it becomes legible. `Files` and `Chunks` read `0`
while the usage history is intact, which is the signature of a rebuilt index
rather than one that never existed:

```
Files:      0
Chunks:     0
Embeddings: 0

Usage (last 7 days)
  search            2 calls
```

The rebuild does log, at `warn`, and the CLI logs at `error` unless `RUST_LOG`
says otherwise:

```
RUST_LOG=warn inkentry index .
```

```
WARN inkentry_core::storage::db: index.db was written by an older schema and cannot be read by this build; rebuilding it empty. Run `inkentry index` to repopulate it. found_version=15 carried_usage_rows=2
```

The fix is `inkentry index .`, which is also what you were going to run anyway.

**Committed tooling: wrong answers rather than failures.** This is the one that
costs a team time, because nothing reports an error. Two output changes reach
anything that parses inkentry:

- `search --format json` and `--format jsonl` emit a nested code/memory
  envelope, not a flat array of results.
- Every memory-entry id is a UUID string. A script reading an integer `id`
  breaks, and an old numeric id no longer resolves.

Both are listed in the changelog under `BREAKING`. Neither is detectable by
running the command and seeing whether it succeeded.

## If you upgraded without exporting

You have not lost anything yet, but stop before doing anything that writes:

1. **Copy `.inkentry/memory.db` somewhere safe now.** It is intact. A refused
   open changes no byte of it.
2. **Do not delete it and re-`init` to make the error go away.** That is the
   step that turns a recoverable situation into an unrecoverable one, and it
   looks like a fix, because search and memory start working again on an empty
   store.
3. **Export the copy, then import the dump.** The export tool reads the file
   directly, so it does not care that the store is no longer in a project this
   build recognises.

If the copy is gone and your memory was published, a fresh `init` recovers what
reached the notes ref, minus the two edge kinds it cannot carry. If it was never
published, the ref holds nothing of yours.

## Teams running a shared server

A team with an explicit `server_url` has a second coordination problem, and
this one really is all-at-once.

The self-hosted `inkentry-server` now identifies memory entries by UUIDv7, and
every route carrying a note id speaks strings where it used to speak integers.
Existing servers upgrade in place: entries predating the change are assigned an
identity on the next start. But a client holding an integer id from an older
server will not resolve it, and there is no mapping and no compatibility path.

So the server and its clients move together. Upgrade the server inside the same
window, not before it and not after it, and treat the CLI upgrade as mandatory
for everyone pointed at that server rather than as something each person
schedules. [Version skew](version-skew.md) covers what the CLI does and does not
tolerate across the rest of the wire contract, and why the general policy is
soft failure rather than a version gate.

Under `mode = "cloud_first"` the server is the store of record, so `inkentry
import` refuses to run: a local write would report success into a store the
project never reads. Import into the local store first, then carry it up:

```bash
INKENTRY_MODE=local_first inkentry import project.dump
inkentry sync
```

## What is not coordinated

Worth stating, so the window does not grow to cover things that do not need it:

- **Re-indexing.** Each person's `index.db` is their own and is gitignored, so
  re-index whenever it suits. On a repository large enough for the embedding
  phase to matter, `inkentry index --detach-embed` parses in the foreground and
  hands the embedding off, so full-text search is back immediately and semantic
  ranking catches up behind it. `inkentry status` reports when it is done.
- **Re-embedding memory.** A dump carries no embeddings, so imported entries
  are not in semantic search until `inkentry memory reindex` has run. Text
  search is phrase-exact, so it is not a substitute; `inkentry status` reports
  the outstanding count as `memory_embedding_pending`.
- **Reading a colleague's notes across the boundary.** The git-notes record
  format is frozen, so the shared ref stays readable in both directions
  throughout the window. A half-upgraded team still sees each other's memory.

## What's next

- [Portable dump format](dump-format.md): what a dump is, and what it carries
- [`inkentry import`](commands.md#inkentry-import): flags, refusals, and what the counts mean
- [Memory](memory.md#sharing-memory-across-clones-via-git-notes): how the shared ref works
- [Stability contract](stability.md#on-disk-formats): what the stores promise from 1.0.0 on
- [Version skew](version-skew.md): CLI and server at different versions
