# ADR-085: `mode` in the project config, and naming the keys that file is not read for

**Date:** 2026-08-17
**Deciders:** architect (this record); founder (Johan) sign-off via PR review
**Relationship to prior ADRs:** extends
[ADR-071](071-per-server-client-bearer-scoping.md) D4, which removed `server_key`
from the checked-in project config and chose silence for a file that still
carries the line. That choice is preserved here, and the reasoning behind it is
what makes the general rule below stop short of it. Operates inside
[ADR-004](004-unified-memory-storage.md)'s "one canonical store per project"
model without reopening it: `mode` is the setting that says which store that is,
and this ADR only changes where the setting may be written.

## Context

### The defect

A team follows the team-server setup and writes one file:

```toml
# .inkentry/config.toml
project_id = "github.com/sass/sass"
server_url = "http://127.0.0.1:7788"
mode = "cloud_first"
```

`inkentry status` reports `mode local_first` and `memory sqlite (local)`. No
warning, no error. `INKENTRY_MODE=cloud_first` in the same project works and
demonstrably moves the store of record, so the value is not wrong, it is
unreachable from this file.

`ProjectConfig` declares four fields (`server_url`, `project_id`, `server_ca`,
`index`), the project merge copies exactly those, and the parse drops anything
else without a word. `mode` was reachable only from the personal
`~/.config/inkentry/config.toml` or from `INKENTRY_MODE`.

The symptom is the worst available one. Memory keeps working, because
`local_first` is a working mode. Nothing looks broken until somebody relies on
the guarantee that the shared server, not each laptop, holds the entries. The
project documentation for `cloud_first` shows exactly the file above, so a team
that does everything right lands here.

### The omission was accidental, and the code says so

The one field excluded from the project config *on purpose* records its reason
in place: `llm_url` is personal-config-or-environment only, because a committed
endpoint points the whole team at one developer's machine, and because it is the
natural sibling of the LLM credential that ADR-071 D4 already excludes.

That reasoning does not reach `mode`. `mode` names no host and carries no
credential. It is a statement about how this project's memory is governed, which
is the same category as `server_url` and `project_id`, both of which are in the
project config and both of which have to be, because they are project identity.
`mode`'s own documentation says an explicit value "pins the mode" and never
names a file, so nothing in the code ever claimed the exclusion was chosen.

### Two questions the obvious fix hides

**"Make `mode` behave like `server_url`" is not a safe instruction.**
`server_url`'s precedence is asymmetric on purpose: the load path discards any
value found in the personal config, so `server_url` is project-file-or-environment
only. `mode` today is the exact inverse, personal-file-or-environment only, with
the project file dropped. Copying `server_url` literally would *remove* the
personal path for `mode`, which works today and is documented as working.

**"Stop dropping unrecognised keys" contradicts a decision already recorded.**
ADR-071 D4 chose silence for a project config that still carries a `server_key`
line, deliberately and against a warn-and-ignore alternative that had been
proposed. The removed `memory_server_*` aliases are silent for the same reason.
A blanket reject would also break a published stability promise: unrecognised
keys are ignored rather than rejected, so a config written for a newer inkentry
still loads on an older one.

## Decision

### D1 - `mode` is a project-config key, layered normally

`mode` joins `server_url`, `project_id`, `server_ca` and `[index]` as a key
`.inkentry/config.toml` is read for. Precedence, highest first:

1. `INKENTRY_NO_SERVER=1`, which forces `offline` and is unchanged.
2. `INKENTRY_MODE`.
3. `.inkentry/config.toml` (project-level, checked in).
4. `~/.config/inkentry/config.toml` (personal).
5. The existing derivation from `server_url`: absent means `offline`, present
   means `local_first`.

The personal path is **kept**, not discarded. Three reasons, in order of weight:

- **`mode` cannot name a destination, and that is the whole of `server_url`'s
  asymmetry.** A personal `server_url` can send a project's memory to a server
  the team never chose. A personal `mode` cannot: with no `server_url` set,
  `cloud_first` still resolves to the local store, because the memory backend
  selector requires both. `mode` can only choose among behaviours toward the
  server the project config already picked, and in the `offline` direction it
  can only choose *less* contact. The danger `server_url`'s rule exists to
  prevent has no analogue here.
- **Removing it would break a published contract.** Which file a key may be set
  in is documented as stable, and `mode` is documented today as a personal-config
  key. Adding a key to the project allowlist is stated to be additive and
  allowed; taking a documented path away is a break. This ADR takes the
  sanctioned half of that pair only.
- **The personal path has a use the environment does not cover well.** An
  explicit `mode = "offline"` makes the capability probe return offline without
  probing, so a developer who does not want a local inference daemon running on
  their machine can say so once, persistently, per machine. `INKENTRY_NO_SERVER=1`
  reaches the same outcome only by living in a shell profile, which is a
  different kind of setting in a different place.

The divergence worry is real but is addressed by ordering rather than by
deletion: a project that states a mode wins over every personal file, so a
developer can no longer quietly run `cloud_first` against a team that decided
`local_first`. A project that states nothing has expressed no governance for a
personal value to contradict.

An invalid `mode` value in the project config is a hard error naming the value,
the file and the accepted set, matching what the personal config and
`INKENTRY_MODE` already do. A mode that cannot be parsed must never fall back to
a default, because a silently defaulted mode is the defect this ADR is fixing,
one layer down.

### D2 - a key the project config is not read for is named on stderr

Loading a `.inkentry/config.toml` emits one stderr line per top-level key the
file is not read for. The line names the key and the file, and the load
continues with that key ignored, exit code unchanged.

**It is a warning, never a refusal.** Unrecognised keys staying loadable is a
stability promise: a config written for a newer inkentry has to keep working on
an older one, and a team's checkouts must not start failing across the board
because one key went stale. A warning closes the gap the defect exposed, which
is that a user cannot tell "configured" from "in effect", without touching what
loads.

**`llm_url` gets its own line**, because the generic one would be wrong about it
in both halves. `llm_url` is not unknown, it is live and excluded on a reason of
its own, and the remedy is not "check the spelling" but "set it in the personal
config or `INKENTRY_LLM_URL`". Its line says so, and states the reason: a
committed endpoint points the whole team at one developer's machine. Every other
unread key, whether misplaced, stale or simply mistyped, gets the generic line,
which names the keys the file *is* read for.

**`server_key` and the `memory_server_*` aliases stay silent**, as ADR-071 D4
decided. The distinction is what the reader could still expect to happen. The
warning exists for a key that names a real setting whose effect did not occur;
these three name nothing anywhere in the product, and their remedy is not a
config edit. For `server_key` it is "rotate the credential, because the
repository's history retains it regardless of what the client now does with the
field", which D4 already established is a documentation matter that no
load-time line can carry. D4's second argument stands too: a permanent runtime
message whose only job is to describe a removed field is code with a
one-release problem and no end date. The exemption is a three-name list beside
the allowlist, and reopening D4 is a separate decision this ADR does not take.

The counter-argument is recorded rather than dismissed: a `server_key` line in a
committed file is a live security problem, and a warning is the only channel
that reaches the person who has one. What decides it here is that the warning
would announce the exposure to every developer on the team, on every command, in
a repository where the credential is already readable by all of them, while
saying nothing they can act on that the docs do not say better. If that trade
is later judged wrong, it is judged on its own terms, in its own record.

### D3 - the allowlist is one list

The keys the project config reads exist once, as a constant, read both by the
merge and by the warning. Two copies would drift, and the failure mode of drift
is precisely this defect: a key that merges but warns, or warns but merges.

## Non-goals

- **Not changing `server_url`'s asymmetry.** A personal `server_url` is still
  discarded. D1 explains why that rule does not generalise to `mode`; it does
  not weaken the rule where it applies.
- **Not reopening ADR-071 D4.** `server_key` is still absent from the project
  config, still unread at every tier, and still silent.
- **Not making `llm_url` take effect from a project file.** It stays excluded
  with its reasoning intact. The only change is that it now says so out loud.
- **Not adding `deny_unknown_fields` anywhere.** Neither config file starts
  rejecting what it does not recognise.
- **Not warning about unread keys in the personal config.** The personal file is
  a superset by design and a per-developer scratchpad; it has no equivalent of
  "a whole team believes this is in effect".

## Consequences

- **The documented team-server setup starts working.** A project config carrying
  `server_url`, `project_id` and `mode` together now does what it reads as
  doing.
- **A project can pin a mode for everyone.** Previously the strongest available
  team-wide statement was `server_url`, and the mode was whatever each developer's
  personal file or environment happened to say.
- **A stale or misplaced key becomes visible.** Every project config carrying a
  key outside the allowlist starts printing one stderr line per command run in
  that project. This is noise by design, and the line names its own remedy.
  Machine-readable output is unaffected: warnings go to stderr and every
  `--format json` surface writes stdout.
- **One asymmetry remains, deliberately.** `server_url` is project-only and
  `llm_url` is personal-only, while `mode` is settable in both. The reason is
  the one D1 gives, and it is now stated on the field rather than left for a
  reader to infer.

## Acceptance criteria

1. `mode` in a project config produces the configured mode, never a silent
   `local_first`.
2. Precedence between project config, personal config and `INKENTRY_MODE` is
   asserted in every combination, including a project file that exists but sets
   no `mode`.
3. A team-server round trip under a project-config `cloud_first` reads and writes
   against the server, proven by a second checkout that never wrote the entry
   listing it under the id the first checkout was handed.
4. A key the project config is not read for is named on stderr, and the load
   still succeeds.
5. `llm_url` in a project config stays excluded and gets its own message.
6. A project config still carrying a `server_key` line keeps working for its
   other fields and stays silent, as do the `memory_server_*` aliases.
7. `INKENTRY_NO_SERVER=1` forces `offline` over a project-config `mode`.
