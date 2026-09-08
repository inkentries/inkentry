# Upgrading to 1.0.0

1.0.0 is the first release that will not open a memory store an earlier build
wrote.

## Credentials do not migrate

The migration brings memory across. It does not bring secrets across: inkentry
stores credentials under its own name, in the OS keychain service `inkentry`
and in `~/.config/inkentry/secrets.toml` when the file store is in use.
Whatever the predecessor stored is still where it was, and inkentry does not
read it.

Nothing is lost, but if you use either of these, set it again before your
first command:

- **A team server's bearer key.** Once per server:

  ```bash
  inkentry auth set-key --server <url>
  ```

  On inkentry cloud, run `inkentry login` instead.

- **An LLM endpoint key**, if you had one:

  ```bash
  inkentry auth set-key --llm
  ```

Both read the value from a prompt or from stdin, never from argv, so it stays
out of shell history and out of `ps` output. In CI or a container, set
`INKENTRY_SERVER_KEY` or `INKENTRY_LLM_KEY` instead of storing anything.

Purely local use needs neither. If you skip this and you did need one, the
symptom is an authentication failure on your first command against the server,
not a migration error.

Writing the key into `~/.config/inkentry/config.toml` as a bare `server_key`
is not a way around this. 1.0.0 no longer reads that field, and a key sitting
in a plaintext file should be rotated rather than moved.

## Run the migration script

Check where memory lives first: `inkentry status`. If it shows a server, skip
straight to [Teams running a shared inkentry-server](#teams-running-a-shared-inkentry-server)
— this section is about the local store.

Otherwise, run the script:

```bash
curl -fsSL https://get.inkentry.com/migrate.sh | sh
```

This is usually the whole job: it installs inkentry, finds every spelunk
store on the machine, exports and imports each one, offers to re-index, and
only then retires spelunk. See [get.inkentry.com](https://get.inkentry.com)
for what it does step by step.

Preview first with no changes made:

```bash
curl -fsSL https://get.inkentry.com/migrate.sh | INKENTRY_MIGRATE_DRY_RUN=1 sh
```

Your `.spelunk` directories are never modified or deleted — they're the
script's own recovery path if something goes wrong.

**Exception:** the script finds stores by scanning for `.spelunk/` directories.
If `.inkentry/memory.db` already exists (you started using inkentry before
running the migration), that scan won't see it, and the script's own
`inkentry import` step fails against it the same way any command does. Export
and swap it in by hand instead:

```bash
spelunk-export --store .inkentry/memory.db --out project.dump
mv .inkentry/memory.db ~/memory-backup.db   # import re-creates the store; it refuses the old one in place, same as any other command
inkentry import project.dump
```

`spelunk-export` is a standalone per-platform download from [the predecessor's
releases](https://github.com/spelunk-cloud/spelunk/releases) — not in the
inkentry archive, since writing a dump means opening a store 1.0.0 refuses to.

Either way, `import` appends the same entries to the shared
`refs/notes/inkentry` git ref. Push that ref — or install the pre-push hook,
`inkentry hooks install --pre-push` — and the rest of the team gets it on
their next `git fetch`/`pull`, so one person can run the import once for
everyone.

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
That's a hard break, not the kind of drift version skew tolerates, so the
server and every client need to upgrade together, in one window, rather than
drifting the way the rest of this doc allows.

**Upgrade the server first.** Its own store (`server.db`) migrates in place —
unlike a client's `memory.db`, there's no export/import step, no refusal, and
no version stamp to check. Redeploy the new `inkentry-server` binary the same
way you deployed it originally (restart the systemd unit, or `docker compose
pull && docker compose up -d` — see [Server setup](server-setup.md)); the
migration ladder runs automatically the moment it opens the store, before it
starts accepting requests.

Once the server's up on the new version, every client upgrades inside the
same window and does its own local migration:

```bash
curl -fsSL https://get.inkentry.com/migrate.sh | sh
```

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
2. **Don't delete it and re-`init`.** That would not be recoverable, and
   would leave you with an empty store.
3. Export it from its new location with `spelunk-export`, then `inkentry
   import` the dump.

No backup, and memory was never published to notes? It's gone. Published? A
fresh `init` recovers what reached the ref, including the `relates_to` and
`contradicts` links between the entries it recovers, and minus anything
recorded with `store_in_git_notes = false`.

## Appendix: symptoms if you skip this

**Memory refusal** (exit `1`, on every command that reads `memory.db`):

```
Error: this memory store was written by an older product (schema version 10) and cannot be opened in place. Export it with `spelunk-export`, then run `inkentry import` on the dump to bring it across.
`spelunk-export` is a separate per-platform download from https://github.com/spelunk-cloud/spelunk/releases — it does not ship with inkentry.
```

The store isn't touched — same bytes before and after.

**The index is rebuilt empty, and says so.** The first command to open an
`index.db` this build cannot read discards it and recreates it at the current
schema, keeping the recorded usage history. That run prints:

```
notice: this index was written by schema version 15 and cannot be read by this build, so it was rebuilt empty (recorded usage history was kept). Run `inkentry index .` to repopulate it.
```

Until you reindex, the emptiness stays attributed rather than looking like an
empty repository. `inkentry search` exits `0` as before, but the line names
the cause:

```
No results found (the index was rebuilt from schema version 15 and not reindexed since, so it holds nothing; run `inkentry index .`).
```

`inkentry status` says the same next to its zeros, and
`inkentry status --format json` carries it as `index_rebuilt_from` (the
discarded version, or `null`). Both go quiet once `inkentry index .` has run.

Fix: `inkentry index .` — which you were going to run anyway.

**Committed tooling gives wrong answers, not errors.** Two output shapes
changed, both listed under `BREAKING` in the changelog: `search --format
json`/`jsonl` now nests a code/memory envelope instead of a flat array, and
every memory-entry id is a UUID string (an old numeric id no longer
resolves). Neither shows up as a failure — a script just gets the wrong
shape back.
