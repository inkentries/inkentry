# ADR-098: A metrics and evaluation system, indexed by commit, computed from state, evals and a local event log

**Date:** 2026-09-17
**Deciders:** Founder (Johan); Architect
**Relationship to prior ADRs:** extends the `usage` table that
`index_001_initial.sql` already describes as "accumulated command telemetry"
and that [ADR-079](079-deprecate-explore-command-and-route.md) relied on. Uses
the `--as-of` read path and the `supersedes` edge
([ADR-086](086-carrier-representation-for-memory-edges.md)) as a source of
evaluation labels. Adds optional fields to the carrier record without touching
identity ([ADR-078](078-uuidv7-memory-entry-identity.md),
[ADR-093](093-entity-id-is-the-portable-handle-for-memory-entries.md)).
[ADR-083](083-memory-relevance-gate-in-unified-search.md) is the motivating
example: a measured regression with an accepted fix that was never implemented,
because nothing re-measures it.

## Context

inkentry records well and cannot show that it does. Three gaps, each verified
against the tree:

1. **No quality measurement runs automatically.** The harness that produced
   ADR-083's numbers lives in `inkentries/inkentry-bench` and is run by hand
   before and after changes; nothing runs in CI. It covers code retrieval
   (CodeSearchNet, call-graph tasks, an own-repo golden set) and agent task
   completion (CrossCodeEval, SWE-bench). Its `memory/` suites
   (`decision_archaeology`, `cross_session_handoff`) exist but produce no
   tracked series. `docs/testing.md` lists KNN ranking precision as not covered.
2. **Usage is almost invisible.** The only instrumentation is
   `usage(command, called_at)` in `index.db`, written from one call site
   (`crates/inkentry-cli/src/cli/cmd/search.rs:103` via
   `storage/stats.rs::record_usage_at`). `context` and `memory` calls are not
   recorded. Nothing records who called (person, agent, hook), what came back,
   or what it cost in tokens.
3. **Entries carry no origin.** A `NoteRecord` has no author, agent or model
   field; `source_ref` is set by `harvest` only (`add.rs` passes `None`). A
   human-written decision and an agent-written one are indistinguishable.

The product direction makes this blocking rather than merely untidy. From this
ADR on, every change is expected to show its effect, even when the only dataset
is inkentry's own development. And the open-source project should be able to publish that series for its own
development.

The model for that report is a training-run dashboard: every chart shares one
x-axis (the training step), headline numbers are shown with their change since
step 1, benchmark versions are named, and incidents are posted in the open. The
decisions below are what is needed for inkentry to produce the same kind of
series honestly.

Two constraints shape everything. The CLI's hot path (`search`, `context`,
`memory`) is parsed by agents and promises deterministic output with no
network; measurement may not add output, latency of note, or egress to it. And
the project's trust position is that code never leaves the machine and only
memory syncs; measurement must not quietly widen that.

## Decision

### D1 - three sources, kept separate

| source | what it answers | instrumentation needed |
|---|---|---|
| **state** | is the log healthy, how big is the review surface | none: derived from `memory.db`, `refs/notes/inkentry` and `git log` |
| **eval** | does retrieval work, did this commit make it better or worse | none in the product: a versioned eval set run against a built binary |
| **events** | do agents and people actually use it, and is use automatic | a local event log (D5) |

A metric belongs to exactly one source. State and eval metrics are reproducible
from a commit and a repository; event metrics are observations and are not.
Reports label which is which.

### D2 - the x-axis is a commit on `main`

Every snapshot (D7) is keyed by commit SHA and carries: inkentry version, eval
set version (D4), embedder model id and quantisation, and the snapshot schema
version. The daily series uses the last commit on `main` that day. A chart may
show dates, but two points are only comparable when their eval set version and
embedder id match; the snapshot records both so a report can refuse to join
across a change, as `paired_stats.py` already refuses to aggregate across
differing cells.

### D3 - metric catalogue v1

Names are stable identifiers. Formulas are exact so two implementations agree.

**Recording (state)**

| id | definition |
|---|---|
| `rec.entries_per_day{kind,origin}` | entries with `created_at` in the day, by kind and by origin (D6; `unknown` until D6 lands) |
| `rec.commit_coverage` | commits on `main` in the window with at least one entry whose `source_ref` or git-notes attachment is that commit, divided by commits in the window |
| `rec.supersede_rate` | entries superseded in the window divided by active decisions at window start |
| `rec.time_to_supersede_p50` | median of `superseder.created_at - superseded.created_at` |
| `rec.open_question_age_p50` | median age of `kind='question'` entries with no `answer` related to them |
| `rec.orphan_rate` | active entries whose every `linked_files` path is absent at the commit, divided by active entries with any `linked_files` |
| `rec.near_duplicate_rate` | active entries having another active entry within cosine distance 0.15 (harvest's existing threshold), divided by active entries |
| `rec.unresolved_conflicts` | count of active pairs joined by a `contradicts` edge, neither superseded |

**Compression (state)** - the review-surface claim

| id | definition |
|---|---|
| `cmp.lines_per_decision` | lines added plus removed on `main` in the window divided by decisions recorded in the window |
| `cmp.review_items_per_day` | entries of kind `decision`, `requirement`, `question`, `antipattern` recorded per day |
| `cmp.tokens_context` | tokens in the default `inkentry context` output at the commit |

Transcript-token compression (decisions per agent-session token) needs session
data and is deferred to the hooks work; it is named here so it is not reinvented.

**Retrieval quality (eval)** - per eval set, per D4

`ret.recall_at_{5,10}`, `ret.mrr_at_10`, `ret.tokens_returned_p50`,
`ret.memory_in_top10` (mean memory entries in the top ten of a code query; the
ADR-083 failure signature), `ret.superseded_leak_rate` (queries whose top ten
contains an entry that was superseded at the as-of time).

**Recollection (events)**

| id | definition |
|---|---|
| `use.sessions_with_context` | sessions with a `context` event in their first three events, divided by sessions |
| `use.search_hit_rate` | `search` events with `memory_result_count + code_result_count > 0`, divided by `search` events |
| `use.search_before_write` | `memory add` events preceded in the same session by a `search` or `context` event, divided by `memory add` events |
| `use.acted_on_rate` | memory entries returned in a session where a file in the entry's `linked_files` is modified before session end, divided by memory entries returned |
| `use.recall_miss_rate` | `question` entries for which, at write time, an active `answer` or `decision` existed within cosine distance 0.15, divided by `question` entries. Computable from state; reported here because it measures recollection failing |

**Automation (events)** - the "a manual step is a bug" rule as a number

`auto.read_rate` and `auto.write_rate`: events with `trigger='hook'` divided by
all read (`search`, `context`) or write (`memory add`, `supersede`) events.

**Visibility (events)**

`vis.seen_within_24h`: agent-origin decisions read by a person within 24 hours
of `created_at`, divided by agent-origin decisions. Requires D6 and a surface
that records a read by a person; not computable from the CLI alone, and named
here so the definition is fixed.

**Outcome** - deferred, named so the series can start later without renaming:
`out.rework_rate` (lines changed again within 14 days), `out.revert_rate`,
`out.review_rounds_p50`, `out.corrections_per_session`. These need weeks of
history and, for the last, session transcripts.

### D4 - versioned eval sets, with labels the product already creates

Eval sets live in `inkentry-bench` under `evalsets/<name>/v<N>/`, are frozen
once published, and are referred to by name and version everywhere
(`memory-supersede/v1`). Changing a set means a new version and a visible break
in the series.

- `memory-supersede/v1`. For each `supersedes` edge in a repository's history:
  the query is the **title** of the superseding entry, the expected result is
  the superseded entry, and the search runs `--as-of` the instant before the
  superseding entry was written, so the answer cannot be the entry that asked
  the question. Body text is excluded from the query because superseding
  entries often quote what they replace.
- `memory-anchor/v1`. For each active entry with validated `linked_files`: the
  query is a path, the expected results are the entries linked to it.
- `memory-commit/v1`. For each entry whose `source_ref` is a commit SHA: the
  files that commit changed are the entry's anchor. The query is one of those
  paths (or the commit subject); the expected result is the entry. This is the
  set that has labels today.
- `decision-archaeology/v1`. The three committed question sets already in the
  bench repo (ripgrep, ruff, tokio), with their blindness protocol.
- `codesearchnet/v1`, `codegraph/v1`. The existing code suites, pinned to a
  seed and sample size so they form a series.

**Label counts today, measured on this repository's `.inkentry/memory.db`
(2026-09-17):** 195 entries (101 decisions, 57 handoffs), 4 `supersedes`
edges, 0 entries with `linked_files`, 181 entries with a `source_ref`. So on
inkentry's own history `memory-supersede/v1` has 4 labels and
`memory-anchor/v1` has none; both are defined now and reported as underpowered
until the log supplies them. `memory-commit/v1` (181 labels) and
`decision-archaeology/v1` carry the memory side of the series at first. The
counts are themselves a finding: 4 supersessions across 101 decisions in three
months says supersession is not happening, which `rec.supersede_rate` will
track.

Source repositories are inkentry's own history first, then any open-source
repository with a harvested log. The label count is recorded in
the set's manifest; a set with fewer than 50 labels is reported but marked
underpowered. Each set records its measured run-to-run noise floor, and a delta
smaller than the floor is reported as no change (ADR-083 measured 0.018 on
Recall@10 for one configuration).

#### What runs when

The full bench suite cannot run per merge: it needs large repository
checkouts, a long `inkentry harvest` preparation pass, a long run, and parts
of it call a model. So the series is built from three tiers, and only the
first is tied to merges.

| tier | when | budget | contents |
|---|---|---|---|
| **T0** | every merge to `main` that touches search, ranking, indexing, embedding or storage code; otherwise the previous point is carried forward | minutes on a hosted runner, no model calls, no external repository checkouts | `memory-commit/v1`, `memory-supersede/v1`, `memory-anchor/v1` on inkentry's own history; `decision-archaeology/v1` against **frozen corpora** (below); `codesearchnet/v1` at a fixed seed and sample (the ADR-083 configuration: a few hundred chunks); the state metrics |
| **T1** | nightly or weekly, scheduled | about an hour, no model calls | `codegraph/v1` on its three repositories with cached indexes; larger CodeSearchNet samples |
| **T2** | before a release, run by hand on a developer machine or a self-hosted runner, as today | hours, and model calls | SWE-bench, CrossCodeEval, and **re-harvesting** the corpora. Results are committed as baselines and appear in the series as sparse points |

The thing that makes T0 possible is separating *evaluating retrieval* from
*evaluating harvest*. Harvest is the slow, model-dependent part, and its
output is just a memory store. Each corpus-based eval set therefore ships with
its harvested store as a frozen artefact (a `dump` file, versioned with the
set). T0 imports the dump, embeds it, and measures retrieval over it: no
model, no preparation pass, no large checkout, and the same input every time. Harvest
quality itself is a T2 question, measured when a new corpus version is cut.

T0's remaining cost is embedding on a CPU runner: the embedder download
(cached between runs) plus a few hundred entries and queries. The budget above
is a target to be confirmed on the first run, and if a set does not fit it
moves to T1 rather than slowing merges.

T0 can only resolve regressions larger than each set's noise floor. That is
the intent: it exists to catch an ADR-083-sized break (Recall@10 0.650 to
0.356) the day it lands, not to rank two good configurations. Fine comparisons
are made with `paired_stats.py` on T1 or T2 runs.

Model-judged evals are allowed as a secondary, periodic report at T2; they
never feed the T0 or T1 series because they are neither deterministic nor free.

### D5 - a local event log, as a table in `memory.db`

One new `events` table in `memory.db`, added as an ordinary migration step
(#292). No new database file: local state should live in as few places as
possible, and a third file would be one more thing to locate, back up and
reason about.
`memory.db` rather than `index.db` because `index.db` is rebuilt rather than
migrated, which is why `usage` has to be specially carried across rebuilds
today. The `usage` table is superseded by `events` and removed once it ships;
`dump` carries `events` as it carries `usage` today.

The table is local working state in the same sense as any other projection
detail: it is not part of the git-notes carrier and no sync path reads it.
`inkentry metrics clear` empties it, which is the whole of the privacy story
a separate file would have given.

```sql
CREATE TABLE events (
    at             INTEGER NOT NULL,
    command        TEXT    NOT NULL,   -- search | context | memory.add | memory.supersede | harvest | sync | ...
    surface        TEXT    NOT NULL,   -- cli | mcp | rest | webui
    trigger        TEXT    NOT NULL,   -- explicit | hook | unknown
    actor_kind     TEXT    NOT NULL,   -- human | agent | unknown
    session_ref    TEXT,               -- opaque, hashed; groups events, identifies nothing
    code_results   INTEGER,
    memory_results INTEGER,
    returned_ids   TEXT,               -- entity_ids of memory entries returned, for use.acted_on_rate
    tokens_out     INTEGER,
    latency_ms     INTEGER,
    ok             INTEGER NOT NULL
);
```

Rules:

- **No query text, no paths, no entry content.** `returned_ids` holds content
  hashes of entries already on the machine.
- `trigger` and `actor_kind` come from the caller declaring itself
  (`INKENTRY_TRIGGER`, `INKENTRY_ACTOR`, set by the hooks and the skill). When
  undeclared they are `unknown`. They are never guessed from a TTY check, since
  a wrong guess would corrupt the automation metric this log exists to produce.
- Writing an event is best-effort and after the response is written, as
  `record_usage_at` is today, with a short busy timeout. `memory.db` is shared
  by every agent and linked worktree, and a read command must never wait on a
  writer to record that it ran: on contention the event is dropped. A failed
  write never fails or delays a command, and nothing is ever added to command
  output.
- The log never enters git notes and never syncs.

### D6 - entries record their origin

`NoteRecord` gains an optional `origin` object: `actor_kind` (`human` | `agent`
| `harvest`), `tool` (for example `claude-code`), `model` (free text). It is
not part of `entity_id`, so identity and dedupe are unchanged, and readers
already ignore unknown keys (`note_record.rs:242`), so older binaries are
unaffected. `memory add` fills it from the same declaration as D5; absent
means `unknown`. The server schema gains the same nullable columns. The
`memory.db` columns arrive as an ordinary `memory.db` migration step (the existing
migration pattern, re-enabled from version 11 by #292); until then `origin` lives in the carrier only.

### D7 - one snapshot document, and it is the only interface to dashboards

`inkentry metrics snapshot --json` emits a deterministic document
(`schema: "inkentry.metrics/1"`) for a commit: the D2 header, then `state`,
`events` and (when supplied by the bench job) `eval` blocks holding the D3
metrics. Given the same repository state and the same `events` rows, it is byte-
identical. The existing `inkentry status` command gains a human summary of the
same data.

Dashboards and reports read snapshots and nothing
else. Nothing downstream queries `memory.db` directly, so the
metric definitions live in one place.

### D8 - nothing leaves the machine

Snapshots are local files and the event log is local. This ADR adds no upload
of either to any server. Sending metrics anywhere would be its own decision,
under the project opt-in of
[ADR-095](095-cloud-is-a-project-opt-in-with-a-fixed-url.md).

The public series for inkentry itself is built only from inkentry's own
repository by a scheduled workflow that publishes the snapshot series and a
feed of recorded entries.

### D9 - ADRs name the metric they expect to move

The ADR template gains a `## Measured by` section: the metric ids from D3 the
change should move, the direction, and the eval set version that will show it.
"Not measurable, because ..." is an acceptable entry; an empty section is not.

### Phasing

1. **Baseline, no product change.** State metrics computed by a script over
   inkentry's own repository; `memory-commit/v1` built, with
   `memory-supersede/v1` and `memory-anchor/v1` defined and reported as
   underpowered; `codesearchnet/v1` pinned; frozen archaeology corpora cut from the
   existing harvested repositories; the T0 job emitting snapshots on merge.
   First result expected: the ADR-083 regression visible as `ret.memory_in_top10`.
2. **Events and origin.** D5 and D6 in the CLI; the summary in `inkentry status`.
3. **Hooks and MCP** declare `trigger` and `actor_kind`; automation and
   recollection metrics become meaningful.
4. **Dashboards and the public report** on top of the snapshot series (D7, D8).
5. **Outcome metrics** once there is history to compute them from.

## Rationale

| Option | Considered | Rejected because |
|---|---|---|
| Third-party product analytics (PostHog, Segment) | Fast to stand up, good dashboards | Sends usage off the machine by default, contradicts the trust position, and cannot compute state or eval metrics at all |
| Count usage server-side only | No client change | Blind to the open-source CLI and to all local work, which is most of the usage; cannot express automation or recollection |
| Extend the `usage` table in `index.db` | Smallest change | `index.db` is a rebuildable projection and `usage` is the one authored table in it; growing it deepens an existing wart |
| A separate `.inkentry/metrics.db` | Obvious privacy story: delete the file and nothing else changes | A third local database to locate, back up and reason about. `metrics clear` gives the same guarantee without a file |
| Date as the x-axis | Simpler to chart | Points are not comparable across embedder or eval-set changes; a commit-keyed snapshot can say when a join is invalid |
| Model-judged quality as the main eval | Closer to "is this decision good" | Non-deterministic and needs model calls on every run, so it cannot run per commit; kept as a periodic secondary |
| Infer human vs agent from TTY or environment | No cooperation needed from callers | Wrong in both directions (agents in PTYs, humans in scripts), and a wrong value silently corrupts the automation metric. `unknown` is honest |
| Hand-labelled memory eval set only | Highest label quality | Does not scale and goes stale. `supersedes` edges and file links are labels the product produces every day; the hand-authored archaeology sets are kept alongside them |

## Consequences

**Easier**

- A change to ranking, fusion, chunking or the embedder has a before and after
  without anyone remembering to run a harness. ADR-083-style regressions are
  visible the night they land, and an accepted-but-unbuilt fix shows up as a
  flat line.
- Comparing inkentry with another tool becomes a run of the same versioned
  sets against it rather than an impression.
- The hooks and MCP work has a target: `auto.read_rate` and `auto.write_rate`
  towards 1.

**Harder**

- Eval sets are now a maintained artefact with versioning discipline, and the
  bench repo becomes part of the release path rather than a side tool.
- Frozen corpora go stale: a harvested store cut today reflects today's
  harvest prompts. That is correct for measuring retrieval and wrong for
  measuring harvest, and the two must not be confused when reading a chart.
  Cutting a new corpus version is a deliberate T2 task, and it breaks
  the series by design.
- `memory-supersede/v1` is only as good as supersede discipline in the source
  history, and today that yields 4 labels; `memory-anchor/v1` yields none
  because no entry carries `linked_files`. The series leans on
  `memory-commit/v1` and the archaeology sets until write-time improvements
  (candidate surfacing on `memory add`, validated file links) start producing
  labels, at which point the label counts become a measure of those
  improvements.
- Two more schemas to keep stable: `events` and the snapshot document.
- Every new surface (MCP, hooks) must declare `surface`, `trigger` and
  `actor_kind`, or the event metrics decay into `unknown`.

**Revisit if**

- `unknown` exceeds roughly a fifth of events after the hooks ship: the
  declaration mechanism is not working.
- The state metrics need more than a few seconds on a large repository: move
  them to an incremental computation.
- A team needs per-user figures: that is a different privacy decision and
  needs its own ADR, not a field added here.

## Security implications

- The `events` table is new local data describing behaviour. It holds no
  query text, paths or content by construction (D5), is never written to the
  carrier, and is never read by a sync path; a test asserts both. `returned_ids` are content
  hashes, meaningful only beside the `memory.db` on the same machine.
  `session_ref` is hashed. It is covered by `dump` and by project removal.
- `egress_containment.rs` must be extended: recording an event and computing a
  snapshot make no network connection.
- The public feed publishes inkentry's own memory entries. Entries can contain
  things not meant for publication (a vulnerability under embargo, a name). The publishing workflow includes only an allowlist of kinds and
  excludes any entry carrying a `private` tag, and security-tagged entries are
  withheld until the fix is released.
- `origin.model` and `origin.tool` are caller-supplied free text and are
  treated as untrusted display data wherever they are rendered.

## Measured by

This ADR is the means of measurement. Its own success criteria: a T0
snapshot series exists for `main`; ADR-083's regression is visible in it;
`unknown` origin falls below a fifth of new entries within one release of D6.
