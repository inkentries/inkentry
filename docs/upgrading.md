# Upgrading to 1.0.0

1.0.0 is the first release that will not open a store an earlier build wrote.
Neither store migrates:

| Store | What 1.0.0 does | Getting back to working |
|---|---|---|
| `.inkentry/index.db` | Discards it, rebuilds empty (keeps only `usage`) | `inkentry index .` — derived from source, nothing lost |
| `.inkentry/memory.db` | Refuses to open it; leaves the file untouched | Export it, then `inkentry import` the dump |

Memory is authored, so nothing regenerates it if it's lost. The refusal itself
is not destructive — but **deleting the store and re-running `init` is**, and
that's the natural instinct once the refusal looks like a broken install.
Don't.

## The sequence

1. **Agree a window and announce it.** State the date and the one rule:
   everyone exports their own memory, from the version they're still running,
   before upgrading anything.
2. **One person goes first.** Upgrade, re-index, `inkentry import` your dump,
   then push — that puts the team's memory on the shared carrier in a form
   1.0.0 reads. Fix any committed script, CI step, or agent instruction that
   depends on the changed output shape, but hold that commit until step 4.
3. **Report back** how long the re-index took on the real repository. That's
   what everyone else plans around.
4. **Everyone else upgrades inside the window**, then re-indexes. Import is
   usually a no-op for them — their entries already arrived via the notes ref
   — but their own dump still matters: it's the only thing carrying
   `relates_to`/`contradicts` edges. Once everyone's across, land the commit
   held back at step 2.
5. **Anyone who was away** exports first, then upgrades, then re-indexes —
   same sequence, just later. This is the step people skip, because by the
   time they're back it feels like catching up rather than a migration.

Running a shared `inkentry-server`? See [Teams running a shared
server](#teams-running-a-shared-server) — that one moves all at once, not in
a window.

## Before you upgrade: get your memory out

```bash
inkentry memory list      # what's in your local store
inkentry status           # where it actually lives
```

If `status` shows memory living on a server, skip to [Teams running a shared
server](#teams-running-a-shared-server) — this section is about the local
store.

**Export it.** `spelunk-export` reads `memory.db` and writes a [portable
dump](dump-format.md). It's a separate download from [the predecessor's
releases](https://github.com/spelunk-cloud/spelunk/releases) — writing a dump
means opening a store 1.0.0 refuses to, so the tool lives on the side that
still can. Bring the dump across with:

```bash
inkentry import project.dump
```

`import` verifies the whole file before writing anything, is idempotent (safe
to run twice), and **publishes**: entries land on `refs/notes/inkentry`, so
anyone who fetches gets them. One person can import for the whole team.

**No export tool handy?** If your memory was ever published to git notes
(`inkentry hooks install --pre-push`, or a manual push of the ref), a fresh
`init` recovers it automatically. Coming from 0.9.8 specifically, fetch the
old ref under its new name first — the predecessor wrote
`refs/notes/spelunk`, 1.0.0 reads `refs/notes/inkentry`:

```bash
git fetch <remote> 'refs/notes/spelunk:refs/notes/inkentry'
```

This fallback loses two things the dump doesn't: `relates_to`/`contradicts`
edges (notes only carry `supersedes`), and anything recorded with
`store_in_git_notes = false`. Good enough to unblock, not a substitute for
the dump.

Either way, `cp .inkentry/memory.db ~/memory-backup.db` costs nothing and
keeps the export option open even after the fact.

## If you already upgraded without exporting

Nothing is lost yet.

1. Copy `.inkentry/memory.db` somewhere safe now.
2. **Don't delete it and re-`init`.** That's the one action that turns this
   from recoverable into not — and it looks like a fix, because search and
   memory both start working again on an empty store.
3. Export the copy with `spelunk-export`, then `inkentry import` the dump.

No copy, and memory was never published to notes? It's gone. Published? A
fresh `init` recovers what reached the ref, minus the two edge kinds above.

## Teams running a shared server

An explicit `server_url` adds a second problem: entries are now identified
by UUIDv7 instead of integers, with no compatibility mapping between them —
a client holding an old integer id won't resolve it against an upgraded
server. Server and clients move together, in one window, not staggered like
the rest of this doc. See [Version skew](version-skew.md) for what does and
doesn't tolerate a mismatch.

Under `mode = "cloud_first"`, `inkentry import` refuses to run locally (the
server is the store of record). Import local first, then push it up:

```bash
INKENTRY_MODE=local_first inkentry import project.dump
inkentry sync
```

## What doesn't need coordinating

- **Re-indexing.** `index.db` is per-machine and gitignored — re-index
  whenever it suits you. `inkentry index --detach-embed` gets full-text
  search back immediately on a large repo while semantic ranking catches up
  behind it.
- **Re-embedding memory.** `import` embeds automatically; if it can't (no
  embedder reachable, or `--no-embed`), it still commits and tells you to run
  `inkentry memory reindex` later. `status` reports the pending count.
- **Reading a colleague's notes across the boundary.** The git-notes format
  is frozen, so a half-upgraded team still sees each other's memory.

## Appendix: symptoms if you skip this

**Memory refusal** (exit `1`, on every command that reads `memory.db`):

```
Error: this memory store was written by an older product (schema version 10) and cannot be opened in place. Export it with `spelunk-export`, then run `inkentry import` on the dump to bring it across.
`spelunk-export` is a separate per-platform download from https://github.com/spelunk-cloud/spelunk/releases — it does not ship with inkentry.
```

The store isn't touched — same bytes before and after.

**Search goes silent, not an error.** `inkentry search` exits `0` with `No
results found.` `inkentry status` shows `Files: 0 / Chunks: 0 / Embeddings:
0` alongside intact usage history — that combination means a rebuilt index,
not a missing one. `RUST_LOG=warn inkentry index .` logs the rebuild:

```
WARN inkentry_core::storage::db: index.db was written by an older schema and cannot be read by this build; rebuilding it empty. Run `inkentry index` to repopulate it. found_version=15 carried_usage_rows=2
```

Fix: `inkentry index .` — which you were going to run anyway.

**Committed tooling gives wrong answers, not errors.** Two output shapes
changed, both listed under `BREAKING` in the changelog: `search --format
json`/`jsonl` now nests a code/memory envelope instead of a flat array, and
every memory-entry id is a UUID string (an old numeric id no longer
resolves). Neither shows up as a failure — a script just gets the wrong
shape back.

## What's next

- [Portable dump format](dump-format.md): what a dump is, and what it carries
- [`inkentry import`](commands.md#inkentry-import): flags, refusals, and what the counts mean
- [Memory](memory.md#sharing-memory-across-clones-via-git-notes): how the shared ref works
- [Stability contract](stability.md#on-disk-formats): what the stores promise from 1.0.0 on
- [Version skew](version-skew.md): CLI and server at different versions
