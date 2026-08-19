# ADR-087: What the project registry is for, and why it stays SQLite

**Date:** 2026-08-19
**Deciders:** founder (Johan), questions of 2026-08-12 and 2026-08-18; architect (this record)
**Relationship to prior ADRs:** scopes the registry that
[ADR-003](003-cross-project-memory-visibility.md)'s cross-project visibility
reads. Does not change what cross-project search surfaces, only what the
registry claims to be.

## Context

Measured on the founder's machine, 2026-08-12:

| | |
| --- | --- |
| registered projects | **319** |
| roots that still exist on disk | **54** |
| roots that are gone | **265** |
| have any authored memory | **3** |

Most registered roots were git worktrees, several of them
`.claude/worktrees/agent-*/...` fixture directories. The product resolves a
worktree to its main checkout elsewhere (`find_project_root` in
`config/paths.rs:50` is worktree-aware and says so), but registration at
`init.rs:91` and `index/phases.rs:323` registers a canonicalised path, and
canonicalisation resolves symlinks, not worktrees.

### The observation that reframes the question

The registry records projects that were **indexed**. A repository whose memory
was only ever added and never indexed is absent from it. The founder's
`handbook`, 343 authored entries and the largest corpus on the machine, is not
in the registry at all. The migration script was built to scan the filesystem
for stores rather than read the registry, deliberately, for that reason.

So the registry cannot answer "what projects does this user have", and it fails
at it silently. That is the finding that decides the rest: the problem is not
mainly that the registry is dirty, it is that it has been read as an inventory
it was never able to be.

### What it is already declared to be

`docs/stability.md:247` already classifies it:

> `~/.config/inkentry/registry.db` | none | **Best-effort**. Tables are created
> idempotently. It holds project registrations, which are re-derivable by
> re-registering.

No versioning, no guarantee, explicitly re-derivable. That classification is
correct and this ADR does not weaken it.

## Decision

**Keep the registry, narrow its stated purpose to the cross-project link graph,
resolve worktrees at registration, prune dead roots lazily on read, and keep it
in SQLite.**

### D1 - it is the cross-project link graph, and it is not an inventory

The registry exists to answer one question: **which other projects should a
cross-project read reach into.** That is what ADR-003 needs and what `links`
and `status --all` display.

It is explicitly **not** an inventory of the user's projects, and nothing may
treat it as one. The `handbook` case is the proof that it cannot be: a corpus
can be arbitrarily large and entirely absent, because presence is a side effect
of having run `index`.

The docs say so plainly, and anything wanting a real inventory scans the
filesystem, as the migration script already does. That is not a workaround to
be tidied away later; it is the correct approach given what the registry is.

### D2 - registration resolves a worktree to its main checkout

Registration goes through the same worktree resolution the rest of the product
uses. A worktree and its main checkout are one project, which is already the
product's position everywhere else; the registry is the one place that
disagrees.

Whether today's 265 dead rows are historic or still being produced needs
establishing during implementation, and the answer decides nothing here: both
answers lead to the same fix, and the eviction in D3 clears the backlog either
way.

### D3 - a row whose root is gone is pruned lazily, on read

A read that encounters a registered root which no longer exists skips it and
removes the row. No maintenance command, no startup scan.

This suits a store already declared best-effort and re-derivable: the cost of a
wrong eviction is one re-registration on next `index`, and the alternative,
carrying 265 dead rows indefinitely because nothing is allowed to forget, is
what produced the current state. Lazy pruning also keeps the cost proportional,
paid only over rows a read actually touches.

**A root that is merely unreachable is not gone.** An unmounted volume, a
network share, or a detached external disk must not be read as deletion. The
check is existence of the project root, and any error that is not a clean
"absent" leaves the row alone.

### D4 - it stays SQLite, and the readability need is met by a command

The founder asked whether a human-readable format such as JSON would serve
better. It would not, and the reason the question is worth answering carefully
is that the underlying want is real.

**Against JSON:**

- **Concurrent writers.** `init` and `index` both register, and a developer or
  CI can run them in several projects at once. SQLite gives that
  serialisation for free. A JSON file is read-modify-write, so concurrent
  registration silently drops one, and fixing it properly means a lock file,
  which is a worse thing to own than the SQLite dependency already linked into
  both binaries.
- **`project_deps` is relational.** The link graph is edges between registered
  projects, which is the shape SQLite is for.
- **No dependency is saved.** sqlite-vec and rusqlite are already linked in
  both binaries for `index.db` and `memory.db`. Removing the registry from
  SQLite removes nothing from the build.

**And the readability benefit is smaller than it looks**, because the store is
already declared re-derivable. Nobody needs to hand-edit a file they can
rebuild by re-registering, and a user who does hand-edit it has no guarantee
they are owed.

**The real need is inspection, and that is a command, not a format.** "I want
to see what is in there and fix it" is answered by `inkentry links` reading
well, supporting `--format json`, and offering a way to drop an entry. Changing
the on-disk format to serve inspection trades a concurrency-safe store for a
human-readable one to solve a problem the CLI should be solving anyway, and it
leaves the inspection story dependent on the user finding a file under
`~/.config/inkentry/`.

If `inkentry links` cannot currently show a user what D1 says the registry is
for, that is the actual gap this question surfaced, and it is worth its own
task.

## Consequences

- **`docs/stability.md:247` gains the scope**: best-effort and re-derivable, and
  explicitly not an inventory of the user's projects.
- **`CLAUDE.md`'s "Multi-project registry" section** describes it as tracking
  "all indexed projects", which is accurate but reads as an inventory. It needs
  the same narrowing.
- **The 265 dead rows clear themselves** through D3 over ordinary use. No
  migration, no one-off cleanup command, consistent with a store nothing
  guarantees.
- **A `links` inspection and removal surface** is the follow-up this raises. It
  is deliberately not folded into this record.

## Whether this is v1

**The scope decision (D1) is v1**, because it is documentation of an existing
surface and because reading the registry as an inventory is how a wrong design
choice gets made downstream. It cost one already, and the migration script only
avoided it because someone checked.

**D2 and D3 are behaviour changes and can follow v1.** They do not alter a
stored format, a wire contract or a documented guarantee, so nothing about
shipping them later is more expensive than shipping them now. That is the test
this store's best-effort classification exists to make easy, and it is why the
"last cheap moment" argument does not apply here the way it does to a format or
a protocol.
