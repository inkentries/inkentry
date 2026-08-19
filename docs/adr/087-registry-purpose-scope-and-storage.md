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

### What it contains, and what puts things there

Measured 2026-08-19, on the machine that has run the product longest:

| | |
| --- | ---: |
| registered projects | 30 |
| of those, throwaway fixtures under agent session scratchpads | 22 |
| real projects | 7 |
| `project_deps` rows | **0** |

**Registration is a side effect of `init` and `index`, and of nothing else**
(`init.rs:91`, `index/mod.rs:153`, `index/phases.rs:323`). `inkentry link`
does not register; it writes `project_deps`. So a user who has never run `link`
still accumulates rows, and any agent that indexes a fixture writes into the
developer's real global registry.

The predecessor's registry still exists beside it at
`~/Library/Application Support/spelunk/registry.db`, with the same two-table
schema, 319 projects and also 0 links. inkentry never opens it.

### Three properties that decide the rest

**It records what was indexed, not what has memory.** Presence is a side effect
of running `index`, so a repository whose memory was only ever added is absent.
Nothing puts a project here on the strength of what it contains.

**It never travels.** No sync mechanism carries it: not the git-notes carrier,
not a team server, not a portable dump. A new machine starts empty and
re-accumulates only what it happens to index.

**It is already declared disposable.** `docs/stability.md:247` classifies it
as **Best-effort**, with no versioning and explicitly re-derivable by
re-registering. That classification is correct and this record does not weaken
it.

Together these mean the registry cannot answer "what projects does this user
have", in principle rather than by accident, and that it fails at it silently.
That is the finding the decisions below rest on: the problem is not that the
registry is dirty, it is that it has been read as an inventory it was never
able to be. That reading has already cost one design decision, and the
migration script avoided it only because someone checked and built a filesystem
scan instead.

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

It is worth recording that the link graph is not currently in use: with
`project_deps` empty in both registries, no cross-project read on this machine
has ever reached anywhere extra. Whether ADR-003's feature earns its keep is a
product question, out of scope here, and it should be taken on that evidence
rather than on an assumption that the table is populated.

### D2 - registration resolves a worktree to its main checkout

A worktree and its main checkout are one project. That is the product's
position everywhere else, and registration must not be the exception.

The evidence says this is already true: 0 of inkentry's 30 registered roots are
worktree-shaped, against a majority of the predecessor's 319. So the
requirement is a **test that asserts it**, not a change that fixes it: register
from inside a linked worktree and assert the row names the main checkout.

If that test fails, route registration through the same resolution
`find_project_dir` uses. Both outcomes are cheap. What is not acceptable is
leaving it unasserted, because the predecessor's registry is a picture of what
this looks like when it regresses.

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
- **The predecessor's registry is not inkentry's to clean.** It is a separate
  file inkentry never opens, and it stays until the user deletes it. A line in
  the upgrade docs, not a code path.
- **Whether the registry should travel between a user's machines is a post-v1
  question**, ruled so by the founder on 2026-08-19. It is raised by D1 rather
  than answered by it: a machine-local store is why this cannot be an inventory
  today, and making it portable would change that premise. Note that the two
  candidate carriers are per-repository (git notes) and per-team (a server),
  while the registry is per-user and spans repositories, so neither fits.

## Whether this is v1

**The scope decision (D1) is v1.** It is documentation of an existing surface,
and reading the registry as an inventory is how a wrong design choice gets made
downstream. It has cost one already.

**D2 is a test rather than a change**, on the evidence that registration
already resolves worktrees. Worth having before v1 only because the
predecessor's registry shows what a regression here looks like.

**D3's guard is a correctness fix and should not wait long.** Removing a
project because its external disk is unmounted is a small, silent loss of
something the user rebuilds by hand. Rare rather than severe, so not
release-gating, but it is the one item here that is a defect rather than
tidying.

None of these alters a stored format, a wire contract or a documented
guarantee, so the "last cheap moment" argument does not apply the way it does
to a format or a protocol. That is what the best-effort classification is for,
and it is why the question this review opened with, whether the registry had to
be settled before v1, resolves to no for everything except the scope narrowing.
