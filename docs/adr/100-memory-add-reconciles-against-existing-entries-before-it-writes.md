# ADR-100: `memory add` reconciles against existing entries before it writes, and the caller resolves what it finds

**Date:** 2026-09-17
**Deciders:** Founder (Johan); Architect
**Relationship to prior ADRs:** extends the write path whose supersede
pre-flight was set by
[ADR-068](068-zero-setup-onboarding-git-notes-memory-fallback.md) E4. Reuses the
distance scale calibrated for
[ADR-083](083-memory-relevance-gate-in-unified-search.md) and depends on the
reserved interactive embed lane of
[ADR-096](096-reserved-interactive-embed-lane.md). Changes the behaviour of
`POST /v1/projects/{id}/notes` and therefore the wire contract owned under
[ADR-076](076-memory-wire-contract-ownership.md). Identity is untouched
([ADR-093](093-entity-id-is-the-portable-handle-for-memory-entries.md)).
Measured through [ADR-098](098-metrics-and-evaluation-indexed-by-commit.md).

## Context

A memory entry is immutable. There is no amend: the only way to change what the
log says is to write a new entry that supersedes an old one. So everything the
writer needs to know about existing entries has to be known **before** the
write.

Today the writer is told nothing.

- `MemoryStore::add_note` (`storage/memory/notes.rs`) is a bare `INSERT`.
  Dedup is the `entity_id` unique index, which catches a byte-identical
  `kind + title + body` and nothing else. Two agents phrasing one decision a
  word apart produce two entries that coexist forever.
- `--supersedes` and `--relates-to` need an id the caller already knows.
  Nothing suggests one. On this repository's own store, 4 of 101 decisions were
  ever superseded in three months.
- The response is `{id, entity_id, kind, title, created}` (`add.rs`).
- The machinery exists elsewhere. `harvest` drops a candidate whose nearest
  neighbour is within distance 0.15. inkentry-server runs a KNN after storing
  (`handlers/notes.rs`, `db.rs::search_notes_for_conflicts`, threshold 0.92
  cosine similarity), writes a `contradicts` edge for each hit and returns
  HTTP 409 with `stored: true`. That edge is never carried back to the client,
  and it labels as a contradiction what it measured as similarity, which is far
  more often a duplicate.
- Embedding happens after the insert, with a five-second budget; on timeout the
  entry is stored with no vector.

The design bet of this product is that the calling agent is the extractor: it
has more context and a stronger model than anything inkentry would run. That
bet only pays if the agent is shown what it might be duplicating, refining or
replacing. Asking it to remember to search first is a manual step, and it does
not happen.

## Decision

### D1 - candidates are computed before the write

`memory add` embeds the new entry first, then looks for candidates among
**active** entries in the project: nearest neighbours by vector distance,
unioned with FTS5 matches on the title. Each candidate is placed in one band:

| band | rule | effect |
|---|---|---|
| **duplicate** | distance below 0.15 (the threshold `harvest` already uses) | blocks the write until resolved (D2) |
| **related** | distance below the ADR-083 calibrated relevance distance | returned with a successful write; does not block |

At most five per band, ordered by distance then `entity_id`, so the output is
deterministic for a given store.

If the embedder cannot answer inside the interactive budget, candidates come
from FTS alone and the write proceeds. A write may be the only copy of a
thought; the embedder never gets to block it.

### D2 - a duplicate-band candidate must be resolved by the caller

When the duplicate band is non-empty and the caller supplied no resolution,
nothing is written. The command exits with a distinct status and returns:

```json
{"created": false, "reason": "candidates",
 "candidates": [{"id": "...", "kind": "decision", "title": "...",
                 "distance": 0.09, "created_at": 1757000000, "band": "duplicate"}]}
```

The caller repeats the command with one resolution per blocking candidate:

- `--supersedes <id>`: this entry replaces that one (existing behaviour).
- `--relates-to <id>`: both stand; record the relation (existing flag).
- `--contradicts <id>`: both stand and disagree; record it (new flag, D4).
- `--distinct-from <id>`: the similarity is incidental. Records nothing.

A resolution naming an id that is not in the current candidate set is accepted;
the set is advice, not a lock. `--no-reconcile` skips D1 entirely for bulk and
scripted imports and is recorded as such in the event log.

### D3 - a successful write returns what is nearby

The success response gains `related: [...]` in the same shape. It is the
write-time answer to "what else should this link to", and it is what the agent
hooks and the MCP surface hand back to the model.

### D4 - similarity stops being called contradiction

`contradicts` is written only when a caller says so, through `--contradicts`
or the equivalent request field. The CLI gains the flag; `--expand-graph`
follows `contradicts` as well as `relates_to`; `context` marks entries that
have an unresolved `contradicts` edge.

inkentry-server stops writing `contradicts` edges from its similarity check.
Its `POST .../notes` adopts D1 to D3: candidates are computed **before**
storing, a blocked write returns 409 with `stored: false` and the candidate
list, and the request gains a `resolutions` field. The capability
`memory.reconcile` on `GET /v1/health` tells a client which behaviour it is
talking to; a client without the capability falls back to today's handling of
409 with `stored: true`.

### D5 - one implementation

`harvest`'s top-1 check and the server's conflict search become callers of the
same candidate function as `memory add`, so the three paths cannot drift.

## Rationale

| Option | Considered | Rejected because |
|---|---|---|
| A separate `memory precheck` command the agent is told to run first | Smallest change, no behaviour break | A step someone must remember. It is the current failure with a new name |
| Supersede automatically above a similarity threshold | Fully automatic | Similarity is not supersession. It would silently archive entries that merely share a subject, and an archive cannot be reviewed by anyone who did not see it happen |
| Run a model in the CLI to judge duplicate, refinement or contradiction | What model-driven memory tools do at write time | The calling agent has the context and is the better model; inkentry would run a smaller one on less information. The CLI stays deterministic and offline |
| Keep the server's store-then-409 | Already shipped | The entry is already in the log when the caller learns of the conflict, and with no amend the only repair is a second entry. It also only runs when writes go to a server |
| Write, then report near-duplicates in a nightly pass | Never blocks a write | Useful as a complement and cheap to add on the same function, but it moves the fix to a human later, which is the review load the product exists to reduce |
| Block on the related band too | More links | Most writes would block. The related band is advice |

## Consequences

**Easier**

- Supersession stops depending on the writer already knowing an id. The
  supersede edges this produces are also the labels for `memory-supersede/v1`.
- Two conflicting decisions can no longer coexist unmarked: the second writer
  is shown the first and has to say how they relate.
- The agent hooks and MCP tool have a concrete loop to automate: write, read
  candidates, resolve, write.

**Harder**

- `memory add` can now return "not written". Every caller, script and skill
  that assumes success needs the new exit status handled. This is a breaking
  change to a stable command and needs the `docs/stability.md` treatment and a
  release note.
- Embedding moves ahead of the insert, so the embedder's latency is felt
  before the write returns rather than after. ADR-096's reserved lane is what
  keeps that bounded.
- Thresholds are now behaviour. 0.15 came from harvest, not from a study of
  agent-written entries.

**Revisit if**

- Agents resolve with `--distinct-from` more than half the time: the duplicate
  band is too wide.
- `rec.near_duplicate_rate` does not fall after release: the band is too
  narrow, or writes are bypassing it with `--no-reconcile`.

## Security implications

- Candidates expose titles of existing entries to the writer. They come from
  the same project store the writer can already search, locally, and on a
  server from within the caller's project scope and the existing key or token
  checks. No cross-project lookup is introduced.
- The secret scan still runs before anything else, so a refused secret is never
  embedded or compared.
- `--no-reconcile` is an explicit bypass, visible in the event log.

## Measured by

`rec.supersede_rate` up and `rec.near_duplicate_rate` down on inkentry's own
history; `rec.unresolved_conflicts` becomes non-zero and then trends to zero;
label count of `memory-supersede/v1` grows. New event metric
`use.reconcile_outcomes`: share of blocked writes resolved as supersedes,
relates-to, contradicts, distinct-from, or abandoned.
