# Upgrading to 1.0.0

1.0.0 is the first release that will not open a memory store an earlier build
wrote. Neither store migrates:

| Store | What 1.0.0 does | Getting back to working |
|---|---|---|
| `.inkentry/index.db` | Discards it, rebuilds empty (keeps only `usage`) | `inkentry index .` — derived from source, nothing lost |
| `.inkentry/memory.db` | Refuses to open it; leaves the file untouched | Export it, then `inkentry import` the dump |

Memory is authored, so nothing regenerates it if it's lost. The refusal itself
is not destructive — but **deleting the store and re-running `init` is**, and
that's the natural instinct once the refusal looks like a broken install.
Don't.

## Run the migration script

Check where memory lives first: `inkentry status`. If it shows a server, skip
straight to [Teams running a shared inkentry-server](#teams-running-a-shared-inkentry-server)
— this section is about the local store.

Otherwise, this is usually the whole job. It installs inkentry, finds every
spelunk store on the machine, exports and imports each one, offers to
re-index, and only then retires spelunk — see [get.inkentry.com](https://get.inkentry.com)
for what it does step by step:

```bash
curl -fsSL https://get.inkentry.com/migrate.sh | sh
```

Preview first with no changes made:

```bash
curl -fsSL https://get.inkentry.com/migrate.sh | INKENTRY_MIGRATE_DRY_RUN=1 sh
```

Your `.spelunk` directories are never modified or deleted — they're the
script's own recovery path if something goes wrong.

**Exception:** the script finds stores by scanning for `.spelunk/` directories.
If `.inkentry/memory.db` already exists (you're on 0.9.8 or a later pre-1.0
build, past the rename but not carried across), that scan won't see it, and
the script's own `inkentry import` step fails against it the same way any
command does. Export and swap it in by hand instead:

```bash
spelunk-export --store .inkentry/memory.db --out project.dump
mv .inkentry/memory.db ~/memory-backup.db   # import re-creates the store; it refuses the old one in place, same as any other command
inkentry import project.dump
```

`spelunk-export` is a standalone per-platform download from [the predecessor's
releases](https://github.com/spelunk-cloud/spelunk/releases) — not in the
inkentry archive, since writing a dump means opening a store 1.0.0 refuses to.

Either way, `import` verifies the whole dump before writing anything and is
idempotent (safe to run twice). It writes to your local `.inkentry/memory.db`
— gitignored, never pushed — and separately appends the same entries to
`refs/notes/inkentry`, a git ref. `git push` (or the `inkentry hooks install
--pre-push` hook) carries *that* ref, not the database file, to everyone who
fetches. One person can run this for the whole team.

## What actually needs coordinating

Running the script, or the manual export, is a per-machine action — nothing
about it needs the team's permission or a shared window. Two things do:

- **Tracked tooling.** `.inkentry/config.toml`, and any committed script, CI
  step, hook, or agent instruction that calls `inkentry`, is tracked and
  reaches everyone the moment it's committed. If one depends on an output
  shape that changed (`search --format json` and every memory-entry id both
  did — see [symptoms](#appendix-symptoms-if-you-skip-this) below), land the
  fix like any other code change: colleagues still on 0.9.x keep working
  against 0.9.x's shapes until they upgrade too, so there's no window to hit.
- **A shared `inkentry-server`.** The one place a real cutover matters — see
  below.

Everything else can happen in any order, at anyone's own pace: the git-notes
carrier's format hasn't changed, so a half-upgraded team keeps reading each
other's memory throughout, and `index.db` is per-machine and gitignored, so
re-indexing never needs coordinating either.

## Teams running a shared inkentry-server

An explicit `server_url` adds a real problem: entries are identified by
UUIDv7 instead of integers, with no compatibility mapping between them — a
client holding an old integer id won't resolve it against an upgraded server.
Server and clients move together, in one window, unlike the rest of this doc.
See [Version skew](version-skew.md) for what does and doesn't tolerate a
mismatch.

Under `mode = "cloud_first"`, `inkentry import` refuses to run locally (the
server is the store of record). Import local first, then push it up:

```bash
INKENTRY_MODE=local_first inkentry import project.dump
inkentry sync
```

## If you already upgraded without exporting

Nothing is lost yet.

1. Move `.inkentry/memory.db` somewhere safe now — a rename, not a copy: the
   path needs to be clear before `inkentry import` can write there.
2. **Don't just delete it and re-`init`.** That's the action that turns this
   from recoverable into not, and it looks like a fix, because search and
   memory both start working again on an empty store.
3. Export it from its new location with `spelunk-export`, then `inkentry
   import` the dump.

No backup, and memory was never published to notes? It's gone. Published? A
fresh `init` recovers what reached the ref, minus edges git notes can't carry
(`relates_to`/`contradicts` — notes only hold `supersedes`) and anything
recorded with `store_in_git_notes = false`.

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
