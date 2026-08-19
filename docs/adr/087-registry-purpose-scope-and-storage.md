# ADR-087: What the project registry is for, and why it stays SQLite

**Date:** 2026-08-19
**Deciders:** founder (Johan), questions of 2026-08-12 and 2026-08-18; architect (this record)
**Relationship to prior ADRs:** scopes the registry that
[ADR-003](003-cross-project-memory-visibility.md)'s cross-project visibility
reads. Does not change what cross-project search surfaces, only what the
registry claims to be.

## Context

> **Correction (2026-08-19): the figures below were measured against the
> predecessor's registry, not inkentry's.** Both files exist side by side on
> macOS. `~/Library/Application Support/spelunk/registry.db` holds **319**
> projects and has not been written since 2026-08-07;
> `~/Library/Application Support/inkentry/registry.db` holds **30**, and 0 of
> them are worktree-shaped. `inkentry autoclean` found 3 stale rows on first
> run and reports all remaining paths valid. The original figures are kept
> because they are what prompted the review, but they describe a store
> inkentry never reads, and the decisions below are revised accordingly.

Measured on the founder's machine, 2026-08-12, **against the predecessor's
registry**:

| | |
| --- | --- |
| registered projects | **319** |
| roots that still exist on disk | **54** |
| roots that are gone | **265** |
| have any authored memory | **3** |

Most of those registered roots were git worktrees, several of them
`.claude/worktrees/agent-*/...` fixture directories.

**In inkentry's own registry, none are.** A query for worktree-shaped roots
returns 0 of 30. So the worktree pollution was the predecessor's, and the
founder's own hypothesis when filing this ("this may have been historic, before
the worktree change") is the correct one.

### What inkentry's 30 rows actually are

Bucketed, 2026-08-19:

| | |
| --- | ---: |
| agent-session scratchpad fixtures | **22** |
| real projects | 7 |
| a macOS `$TMPDIR` directory | 1 |
| **`project_deps` rows** | **0** |

The 22 are throwaway test projects created inside per-session scratchpad
directories by agents running `init` or `index` while testing: `sandbox/projA`,
`sandbox2/projB`, `sandbox3/notgit`, `sb5/proj`, `sb7`, `projB-target`, across
four distinct session ids. They are not worktrees and never were repositories
anyone worked in.

Two facts follow, and both matter more than the dead-row count this task was
filed about.

**Registration is a side effect of `init` and `index`, and of nothing else.**
Three call sites, `init.rs:91`, `index/mod.rs:153` and `index/phases.rs:323`.
`inkentry link` does not register; it writes `project_deps`. So a user who has
never run `link` still accumulates rows, and every agent that indexes a fixture
writes into the developer's real global registry.

**`project_deps` is empty in both registries.** Not a rename artifact:
inkentry's registry is only weeks old, but the predecessor's carries the same
two-table schema, 319 projects, and **0 links**.

Scope that claim carefully. **The registry is machine-local and travels through
no sync mechanism**, so both files describe one machine, and the founder changed
machines around the rename. Links created on an earlier machine would be
invisible here. What the evidence supports is that `link` has not been used on
this machine across 349 registered projects; it does not support "never, by
anyone, anywhere".

That the registry does not travel is itself the more useful finding, and it is
independent of any count. See D1.

### The observation that reframes the question

The registry records projects that were **indexed**. A repository whose memory
was only ever added and never indexed is absent from it, because presence is a
side effect of running `index` rather than of having anything worth recording.

The example originally given for this does not hold: the founder's `handbook`
was cited as absent, and it **is** registered in inkentry's registry. That
citation was against the predecessor's file. The structural point survives the
example, and it is the part that matters: nothing registers a project on the
strength of its memory, so a memory-only project is still invisible here by
construction. The migration script was built to scan the filesystem for stores
rather than read the registry, deliberately, for that reason.

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

**The decisive property is that the registry never travels.** It is written only
by `init` and `index` on the machine they run on, and no sync mechanism carries
it: not the git-notes carrier, not a team server, not a portable dump. A new
machine, or a machine change, starts empty and re-accumulates only what it
happens to index.

So it cannot be an inventory of the user's projects even in principle, whatever
it happens to contain. That argument holds without appealing to any count, and
it is the one to keep: the counts were measured wrong once already.

It is also worth being honest that it is not currently doing its actual job
either. `project_deps` is empty in this registry and in the predecessor's
319-project one, so no cross-project read on this machine has reached anywhere
extra.
The `projects` table still earns its place, because `find_project_for_path`
resolves a directory to its project and `link` needs that to work at all. But
the link graph is a capability the product offers rather than one in use, and
a decision about ADR-003's future should be taken on that evidence rather than
on the assumption that the table is populated. That question is out of scope
here and belongs to whoever owns cross-project visibility.

It is explicitly **not** an inventory of the user's projects, and nothing may
treat it as one. Presence is a side effect of having run `index`, so a corpus
can be arbitrarily large and entirely absent. Nothing puts a project here on
the strength of its memory.

The docs say so plainly, and anything wanting a real inventory scans the
filesystem, as the migration script already does. That is not a workaround to
be tidied away later; it is the correct approach given what the registry is.

### D2 - registration resolves a worktree to its main checkout, and the evidence says it already does

A worktree and its main checkout are one project. That is the product's
position everywhere else, and registration must not be the exception.

**The observed pollution was the predecessor's, not inkentry's.** 0 of
inkentry's 30 registered roots are worktree-shaped, against a majority of the
predecessor's 319. So this is very likely already correct and the requirement
here is to **confirm it with a test rather than change behaviour**: register
from inside a linked worktree and assert the row names the main checkout.

If that test passes as written, this decision is satisfied by what is already
in the tree and nothing more is owed. If it fails, the fix is to route
registration through the same resolution `find_project_dir` uses. Both outcomes
are cheap; what is not acceptable is leaving it unasserted, because the
predecessor's registry shows exactly what this looks like when it regresses.

### D3 - eviction stays in `autoclean`, and is not moved into the read path

**`inkentry autoclean` already exists** (`cli/cmd/link.rs:108`,
`registry.rs:302`) and already does this: it drops any project whose root no
longer exists, and additionally removes a leftover `.inkentry` directory when a
worktree was cleaned but its ignored directory survived. On the founder's
machine it removed 3 rows and now reports all 30 remaining paths valid.

So the question is not whether to add eviction. It is whether eviction should
**also** happen automatically on read. The answer is no, for now:

- The measured backlog in inkentry's registry is **3 rows, not 265**. The
  problem lazy pruning was proposed to solve is not present at this scale.
- An automatic prune on read makes every cross-project read a potential
  **write**, on a store shared by concurrent `inkentry` processes. That is a
  real cost to pay for tidiness nobody has asked for.
- Eviction on read is also where the unreachable-versus-deleted distinction
  below becomes dangerous, because it fires without the user having asked for
  anything.

Revisit if a real registry accumulates dead rows faster than users run
`autoclean`. Nothing measured so far suggests it does.

**A root that is merely unreachable is not gone**, and this applies to
`autoclean` as it stands today. An unmounted volume, a network share, or a
detached external disk must not be read as deletion. `autoclean` currently
tests `!p.root_path.exists()`, and `Path::exists` returns `false` for **any**
error, including a permission denial or an unmounted mount point, so it cannot
today distinguish "absent" from "cannot tell". Removing a row for a project
sitting on an external disk is silent data loss of a link the user has to
rebuild by hand.

That is the one change this decision does require: the check must treat a clean
"absent" as absent and leave the row alone on anything else.

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
- **`autoclean` gains an unreachable-versus-absent guard**, per D3. That is the
  only code change this record asks for, and it is a correctness fix rather
  than tidying: today an unmounted volume reads as a deleted project.
- **The predecessor's 319-row registry is not inkentry's problem to clean.** It
  is a separate file that inkentry never opens, and it will sit at
  `~/Library/Application Support/spelunk/registry.db` until the user deletes
  it. Worth a line in the upgrade docs, not a code path.
- **A `links` inspection and removal surface** is the follow-up this raises. It
  is deliberately not folded into this record.
- **Whether the registry should travel between a user's machines is a post-v1
  question**, ruled so by the founder on 2026-08-19. It is raised by D1 rather
  than answered by it: a machine-local store is why this cannot be an inventory
  today, and making it portable would change that premise. Anyone taking it on
  should note that the two candidate carriers are per-repository (git notes) and
  per-team (a server), while the registry is per-user and spans repositories, so
  neither is an obvious fit.

## Whether this is v1

**The scope decision (D1) is v1**, because it is documentation of an existing
surface and because reading the registry as an inventory is how a wrong design
choice gets made downstream. It cost one already, and the migration script only
avoided it because someone checked.

**D2 is a test, not a change**, on the evidence that registration already
resolves worktrees. Cheap either way, and worth having before v1 only because
the predecessor's registry is a picture of what a regression here looks like.

**D3's guard is a correctness fix and should not wait long.** `autoclean`
removing a project because its external disk is unmounted is a small, silent
loss of something the user has to rebuild by hand. It is rare rather than
severe, so it is not release-gating, but it is the one item here that is a
defect rather than tidying.

Neither alters a stored format, a wire contract or a documented guarantee, so
the "last cheap moment" argument does not apply the way it does to a format or
a protocol. That is what this store's best-effort classification is for, and it
is why the original review question, whether this had to be settled before v1,
resolves to no for everything except the scope narrowing.
