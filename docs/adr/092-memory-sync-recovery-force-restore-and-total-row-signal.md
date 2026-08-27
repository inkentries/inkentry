# ADR-092: Memory-sync recovery: force-restore push and the total-row divergence signal

**Date:** 2026-08-27
**Deciders:** Founder (Johan); Architect
**Relationship to prior ADRs:** extends
[ADR-037](037-local-first-memory-sync-and-relay.md) (the sync and cursor model),
[ADR-076](076-memory-wire-contract-ownership.md) (the memory wire contract this
repo owns) and [ADR-078](078-uuidv7-memory-entry-identity.md) (UUIDv7 entry
identity). It does not change [ADR-069](069-git-notes-sharing-pre-push-hook-and-tracking-refspec.md)'s
carrier or refspec, or [ADR-059](059-git-notes-v1-format-freeze.md)'s frozen
note format.

## Context

`inkentry sync` and `inkentry plumbing push` decide an entry is already synced
purely from the local column `notes.remote_id IS NOT NULL`. That is unsound for
two reasons: `remote_id` (the server's own previously-minted UUIDv7) rides
verbatim in the git-notes carrier, so an entry re-imported via `inkentry index`
arrives already bearing one; and it is not scoped to a server. After a server
loses its database, the client still believes everything is synced and pushes
nothing, while a forward-only `since_id` catch-up (`id > cursor`) cannot discover
rows that would sit behind the cursor. The result is a silent, permanent
local/server divergence with no recovery path.

The primary real-world exposure is a self-hosted team server (a crash or a
dropped database). Managed cloud is not expected to lose its database. Because
the CLI is one client that must behave identically against a self-hosted team
server and against managed cloud, the team-server API changes below use field
names and semantics that are identical to the cloud server's implementation of
the same contract.

Current behaviour (read from `main`):

- Team-server batch ingest takes `BatchNoteItem`s keyed on a required
  `external_id` with no `id` field; the server mints a UUIDv7 (`Uuid::now_v7()`),
  returns it in `BatchItemResult.id`, and the CLI stores it as `remote_id`.
  Idempotency is on `(project_id, external_id)`: a live note already carrying the
  `external_id` is skipped, not duplicated.
- The `/memory/since` handler's `since_id` delta-pull mode returns
  `SinceIdResponse { entries, count }`, where `count` is the length of this
  response page; the legacy `t` timestamp mode returns a bare array. `since_id`
  is a UUIDv7 cursor and entries with `id` greater than the cursor are returned.

## Decision

Three paired changes. The normal (non-force) sync path is untouched on both the
client and the server. Only `inkentry plumbing push --force` exercises the new
input.

### A. CLI: `inkentry plumbing push --force`

- With `--force`, bypass the `remote_id IS NOT NULL` skip and re-offer every
  active entry.
- For each entry, send its existing `remote_id` (the server's own
  previously-minted UUIDv7, cached in `memory.db` and in the git-notes carrier)
  as the ingest `id`. The CLI never sends its local-only identity as the server
  id; it hands the server back the server's own prior id, so identity is
  restored rather than reassigned.
- Rely on the server's `(project_id, external_id)` idempotency: missing entries
  are created under their original id, present ones are skipped, and a healthy
  server sees no duplicates.
- Report these outcomes as `created` / `skipped` in the JSONL report, not as
  `already_synced`.
- Normal `sync` and non-force `push` omit `id` and are unchanged.

### B. Team-server ingest honors an optional client-supplied `id` on create

Add one optional field to the ingest item:

```
BatchNoteItem.id : Option<String>   // JSON: optional; a well-formed UUIDv7
```

Behaviour:

- **Absent** (every normal write): the server mints a UUIDv7 (`Uuid::now_v7()`)
  exactly as today.
- **Present and a well-formed UUIDv7**, on the create branch (the
  `(project_id, external_id)` pair is new for this project): the row is inserted
  under the supplied id instead of a minted one, restoring its original identity.
- **Present and not a well-formed UUIDv7** (not a UUID, or a UUID whose version
  is not 7): the request is rejected `400 Bad Request` and nothing in the batch
  is written, in the same class as the existing empty-`external_id` validation.
  A malformed id is never silently ignored and minted, so a client bug surfaces
  loudly instead of producing a divergent identity.
- **Idempotency is unchanged.** If a live note already exists for
  `(project_id, external_id)`, that note is skipped and the supplied `id` is
  ignored entirely. A supplied `id` can never override, mutate, or re-key a
  stored note; it is consulted only when creating a genuinely new row.
- **Uniqueness is unchanged.** The supplied `id` must be unique as the primary
  key has always been; a collision with a different existing row is reported as
  that entry's `failed` result through the existing constraint path, with no new
  override behaviour.

"Well-formed UUIDv7" means the value parses as a UUID and its version field is 7
(ADR-078). The batch response already returns the per-entry id in
`BatchItemResult.id`.

### C. Team-server since returns an active-note total, and the CLI compares it

Add one additive field to the `since_id` delta-pull response:

```
SinceIdResponse {
  entries : [ ... ],   // unchanged
  count   : <page length>,   // unchanged
  total   : <COUNT of active notes for the project>   // NEW
}
```

- `total` is always present in the `since_id` mode and is counted server-side as
  the true active-note count for the project, consistently with the active set
  the client materialises locally. `count` keeps its meaning (page length).
- The legacy `t` timestamp mode keeps its bare-array response and gains no field.
- There is no change to `since_id` cursor semantics: cursor computation, entry
  ordering, and the `id > since_id` forward scan are untouched.
- CLI: after applying a `since_id` pull, compare the local active-note total to
  the response `total`. When the server's `total` is higher, `since_id` cannot
  be trusted (rows exist behind the cursor), so re-pull the whole dataset,
  ignoring the cursor.

The request field is named `id` and the response field is named `total` on both
the team server and the cloud server, so the CLI speaks one contract to either
backend.

## Rationale

Restoring under the original id rather than minting a new one is the
load-bearing choice. The original UUIDv7 rides in every machine's git-notes
carrier, so re-minting on recovery would rewrite one machine's carrier with new
ids while the rest still hold the old ones, scattering identity across the fleet.
Restoring under the sent id keeps every carrier consistent and leaves other
clients' `since_id` cursors valid, so no cursor reconciliation is required.
Because ids and ordering are preserved, `total` is a divergence signal rather
than a cursor fix: a restored row carries its original (older) id and therefore
lands behind the cursor of any client already advanced past it, where a forward
`id > since_id` scan never reaches it. The `total` mismatch is what makes such a
client fall back to a full pull and pick the row up, so `--force` and the `total`
field ship as a pair.

| Option | Considered | Rejected because |
|---|---|---|
| Server adopts the CLI's local entry id | yes | The local id must never become the server id. The value re-sent under `--force` is the server's own prior id (the entry's `remote_id`), handed back to it. |
| Server-scoped `remote_id` | yes | Too much machinery for a scenario that, in managed cloud, should not occur, and it does not by itself restore rows a fresh database never had. |
| Manifest reconciliation between client and server | yes | Heavier two-way negotiation; a one-way active-note `total` on an existing response is the low-cost signal that achieves recovery. |
| Count rows before the cursor instead of the project total | yes | Cursor-relative counting is directional and fragile; a project-wide active `total`, counted the same way on both sides, detects the actual case (server holds more, behind the cursor). |
| Put the recovery flag on `sync` rather than `plumbing push` | yes | `plumbing push` is the low-level agent surface, so `--force` is hidden enough that ordinary users do not run it habitually while operators keep a recovery path. |

## Consequences

- Easier: a team-server operator can recover a lost dataset by running
  `inkentry plumbing push --force`, and the rest of the fleet self-heals on its
  next pull.
- Additive on the wire: `total` is a new field on the existing `since_id`
  response and `id` is a new optional input, so older clients and older servers
  interoperate unchanged.
- Harder or to watch: the ingest create path gains an id-provenance branch and a
  UUIDv7 validation step, and each `since_id` response computes one active-note
  count; both stay within paths that already touch the same tables.
- Out of scope: any change to the normal (non-force) sync path; server-scoped
  `remote_id`; the server adopting the CLI's local id; manifest reconciliation.
- Revisit if: entry identity moves off UUIDv7, or the `since_id` cursor model
  changes, either of which reopens the validation rule or the divergence signal.

## Security implications

The supplied `id` is a create-only input bounded by the server's existing
project scoping and idempotency: it is honored only for the authenticated
project, only when `(project_id, external_id)` is new, and never to override or
mutate a stored note, so it opens no cross-project or overwrite surface. A
malformed id is rejected rather than coerced, and a colliding id fails that one
entry through the existing constraint path. `total` exposes only an aggregate
active-note count for a project the caller may already read, and leaks no note
contents. `--force` is gated behind the low-level `plumbing` surface, so the
re-offer-everything path is a deliberate operator action rather than a default.
