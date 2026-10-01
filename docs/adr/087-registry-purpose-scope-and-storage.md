# ADR-087: What the project registry is for, and why it stays SQLite

**Date:** 2026-08-19
**Deciders:** founder (Johan); architect (this record)
**Relationship to prior ADRs:** scopes the registry that
[ADR-003](003-cross-project-memory-visibility.md)'s cross-project visibility
reads. It does not change what cross-project search surfaces, only what the
registry claims to be.

## Context

`~/Library/Application Support/inkentry/registry.db` holds two tables:
`projects`, a path-to-database mapping, and `project_deps`, the links between
them that cross-project reads follow.

### What puts things in it

**Registration is a side effect of `init` and `index`, and of nothing else**
(`init.rs:91`, `index/mod.rs:153`, `index/phases.rs:323`). `init` registers
unconditionally, including under `--no-index` and before the database exists.
`inkentry link` does not register at all; it writes `project_deps`.

Two consequences follow. A user who has never run `link` still accumulates
rows, so the `projects` table fills up on its own while the link graph stays
empty. And any process that runs `init` or `index` in a throwaway directory
writes into the developer's real global registry, which is how agent test
fixtures come to be registered alongside real work.

### Two properties that decide the rest

**It never travels.** No mechanism carries it: not the git-notes carrier, not a
team server, not a portable dump. It is written only by `init` and `index` on
the machine they run on, so a new machine starts empty and re-accumulates only
what it happens to adopt there.

**It is already declared disposable.** `docs/stability.md:247` classifies it as
**Best-effort**, with no versioning and explicitly re-derivable by
re-registering. That classification is correct and this record does not weaken
it.

A machine-local store that nothing carries describes a machine, not a user. So
the registry cannot answer "what projects does this person have" in principle
rather than by accident, and it gives no sign that it cannot. That is the
finding the decisions below rest on, and it is why the migration script scans
the filesystem for stores rather than reading the registry: a migration is
looking for stores that predate inkentry entirely, which by construction were
never registered here.

## Decision

**Keep the registry, narrow its stated purpose to the cross-project link graph,
resolve worktrees at registration, leave eviction in `autoclean`, and keep it in
SQLite.**

### D1 - it is the cross-project link graph, and it is not an inventory

The registry exists to answer one question: **which other projects should a
cross-project read reach into.** That is what ADR-003 needs, and what `links`
and `status --all` display.

It is explicitly **not** an inventory of the user's projects, and nothing may
treat it as one. The argument is the not-travelling property above, which holds
whatever the table happens to contain: a machine-local store that no mechanism
carries cannot describe a user, only a machine. Anything wanting a real
inventory scans the filesystem, as the migration script does. That is the
correct approach given what the registry is, not a workaround to be tidied away
later.

The `projects` table earns its place regardless, because
`find_project_for_path` resolves a directory to its project and `link` cannot
work without it.

**The two tables have different futures, and this record only settles the
first.** `projects` is machine-local by nature: it maps this machine's paths to
this machine's databases, and there is nothing in it another machine could use.
`project_deps` is not obviously the same. A link between two projects is a
statement about a person's or a team's work rather than about one checkout, and
if it should ever follow a user across machines, a team server is the mechanism
that already exists for carrying shared state. That makes link portability a
question for the team-server surface, after v1, rather than a property of this
store.

Recorded because the table is empty today, so the question has never been
forced: no cross-project read has yet reached anywhere extra. Whether ADR-003's
feature earns its keep should be judged on that, and it is out of scope here.

### D2 - registration resolves a worktree to its main checkout

A worktree and its main checkout are one project. That is the product's
position everywhere else, and registration must not be the exception.

Nothing currently asserts it. Registration canonicalises the project root
(`init.rs:87`), and canonicalisation resolves symlinks, not worktrees, so the
guarantee rests on callers happening to pass an already-resolved path rather
than on the registration path enforcing it.

The requirement is therefore a **test**: register from inside a linked worktree
and assert the row names the main checkout. If it passes, this decision is
already satisfied by the tree and nothing more is owed. If it fails, route
registration through the same resolution `find_project_dir` uses. Both outcomes
are cheap; leaving it unasserted is what is not acceptable, because a
regression here is invisible until someone reads the table.

### D3 - eviction stays in `autoclean`, and does not move into the read path

`inkentry autoclean` (`cli/cmd/link.rs:108`, `registry.rs:302`) already drops
any project whose root no longer exists, and additionally removes a leftover
`.inkentry` directory where a worktree was cleaned but its ignored directory
survived.

So the question is not whether to add eviction, but whether it should **also**
happen automatically on read. It should not:

- An automatic prune makes every cross-project read a potential **write**, on a
  store shared by concurrent `inkentry` processes.
- The measured backlog is single digits. The problem an automatic prune would
  solve is not present at this scale.
- Eviction without the user asking for it is where the distinction below
  becomes dangerous.

**A root that is merely unreachable is not gone.** `autoclean` tests
`!root_path.exists()`, and `Path::exists` returns `false` for **any** error,
including a permission denial or an unmounted mount point. A project on a
detached external disk is therefore removed as though it had been deleted, and
the user rebuilds the link by hand. The check must treat a clean "absent" as
absent and leave the row alone on anything else.

That guard is the only code change this record asks for.

### D4 - it stays SQLite, and the readability need is met by a command

A human-readable format such as JSON would not serve better, though the want
behind the question is real.

- **Concurrent writers.** `init` and `index` both register, and a developer or
  CI can run them in several projects at once. SQLite serialises that for free.
  A JSON file is read-modify-write, so concurrent registration silently drops
  one, and fixing it properly means owning a lock file, which is worse than a
  dependency both binaries already link.
- **`project_deps` is relational.** The link graph is edges between registered
  projects, which is the shape SQLite is for.
- **No dependency is saved.** rusqlite and sqlite-vec are already linked in both
  binaries for `index.db` and `memory.db`.
- **The readability benefit is small**, because the store is already declared
  re-derivable. Nobody needs to hand-edit a file they can rebuild by
  re-registering.

**The real need is inspection, and that is a command, not a format.** "I want to
see what is in there and fix it" is answered by `inkentry links` reading well,
supporting `--format json`, and offering a way to drop an entry. Changing the
on-disk format to serve inspection trades a concurrency-safe store for a
human-readable one, and still leaves the user hunting for a file under the
config directory.

If `inkentry links` cannot show what D1 says the registry is for, that is the
gap this question surfaced, and it is a follow-up rather than part of this
record.

## Consequences

- **`docs/stability.md:247` gains the scope**: best-effort, re-derivable, and
  explicitly not an inventory of the user's projects.
- **`CLAUDE.md`'s "Multi-project registry" section** describes it as tracking
  "all indexed projects", which is accurate but reads as an inventory. It needs
  the same narrowing.
- **`autoclean` gains the unreachable-versus-absent guard** from D3. A
  correctness fix, not tidying: today an unmounted volume reads as a deleted
  project.
- **A `links` inspection and removal surface** is the follow-up D4 raises.
- **Whether `project_deps` should follow a user across machines is a post-v1
  question for the team-server surface**, per D1. `projects` is not part of
  that question: it maps this machine's paths to this machine's databases and
  has nothing another machine could use.
- **None of these decisions alters a stored format, a wire contract or a
  documented guarantee.** They are free to land in any order and at any time,
  which is what the best-effort classification exists to buy. Sequencing is a
  scheduling question and does not belong in this record.
