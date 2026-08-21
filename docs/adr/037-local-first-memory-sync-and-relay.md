# ADR-037: Local-first team memory sync and the local relay

**Date:** 2026-06-18 (P1 sync model); 2026-07-19 (P2 local relay)
**Deciders:** founder (Johan); architect
**Record scope:** This is the OSS-side record of the sync model and its P2 local
relay. It exists so every `ADR-037` and `ADR-037 P2` reference in this repo resolves
in-repo. A shared team server may be the hosted cloud API or another
`inkentry-server`; the cloud-only parts of the original decision (CLI login and
identity) are not recorded here because they change nothing in this codebase.
**Relationship to prior ADRs:**

- [ADR-004](004-unified-memory-storage.md): `memory.db` is the one canonical store.
  This ADR is the team-sync design ADR-004's 2026-07-19 and 2026-07-23 amendments
  defer to; those amendments already carry the store-of-record and sync-relay-role
  consequences and are the companion to this record.
- [ADR-078](078-uuidv7-memory-entry-identity.md): the stable UUIDv7 entry identity
  that makes two-way sync idempotent (the linchpin, D2).
- [ADR-056](056-oss-server-tenancy-model.md): the keyless loopback posture the relay
  surface lives inside.
- [ADR-091](091-relay-discovery-trusts-only-the-recorded-daemon.md): hardens which
  local daemon the CLI will hand a body or bearer to; operates inside this relay
  contract without changing what it sends.
- [ADR-089](089-default-port-range.md): the default port the relay reuses.

## Context

Memory is local-first: a project's authoritative store is its local `memory.db`
(ADR-004). A team shares memory only when a `server_url` is explicitly set, and how
it moves is governed by `mode` (`SyncMode`,
`crates/inkentry-core/src/config/sync_mode.rs`). Before this decision three gaps
remained: `sync` was a one-way, non-idempotent push seed (it re-POSTed every row,
dropped `supersedes`, and propagated no archives); "offline vs online" was decided
per-process by the tier probe with no persistent per-project control; and there was
no home for the design of a real two-way syncer or for the process that runs it in
the background.

## Decision

Activate local-first two-way sync in two phases. Local `memory.db` stays the source
of truth for reads; a team server is a converging replica, never a hard dependency
for local work.

### P1 - the sync model

**D1. Sync-mode flag.** `mode: SyncMode` on the config, overridable by `INKENTRY_MODE`:

| mode | Reads | Writes | Team-server contact |
|---|---|---|---|
| `offline` | local | local | never, even if `server_url` is set |
| `local_first` *(default when `server_url` is set)* | local | local, then async background sync | best-effort |
| `cloud_first` *(debug/override only)* | server, local fallback | server, queue locally if unreachable | required |

No `server_url` defaults to `offline`; a set `server_url` defaults to `local_first`.
`cloud_first` is server-authoritative for that invocation only: a deliberate,
explicit override of local-as-source-of-truth, not a day-to-day mode.

**D2. Two-way sync engine.** `sync` is push and pull, keyed on a stable identity:

- Stable identity (linchpin). Entries carry a UUIDv7 (ADR-078); it correlates local
  and remote rows and makes sync idempotent.
- Cursor is the synced UUID, not a wall clock. A timestamp watermark is frail under
  clock drift; because UUIDv7 sorts time-ascending, the cursor is the max
  already-synced remote id (`MAX(remote_id)`). Push sends the `remote_id IS NULL` set
  and records the ids the server assigns; pull reads everything after the cursor. No
  separate `last_synced` table.
- Conflict policy is keep-both, never last-writer-wins. Entries are append-only and
  supersede-only, so concurrent conflicting writes both survive and a `contradicts`
  edge is recorded for human resolution; identity wins for dedupe, semantic
  similarity only flags.
- `sync` propagates `supersedes` and archive/tombstone, which the push seed dropped.

**D3. Text-only on write.** The CLI ships entry text; the server backfills the
embedding with its own configured model. Raw entry text is canonical; the embedding
is a derived, rebuildable index, never primary data. This removes the
embedding-space-mismatch class (a client vector could live in a different space than
the server's query embeddings) and makes a server model change a re-embed of stored
text, not a data-loss event. There is no client-vector send path and no
`--client-embed` escape hatch.

### P2 - the local relay (the surface this ADR is cited for)

P2 needs a persistent process to hold a non-blocking outbox drain and a live pull. A
per-invocation CLI writes and exits, so it can host neither; the only persistent
client-side process is the local `inkentry-server`. P2 gives that server a
sync-relay role without making it a memory store.

**D5. Process boundary: relay, not shared-file access.** The local server hosts the
reconciler's network legs only and never opens a project's `memory.db`.

| Concern | Owner |
|---|---|
| Draining the write outbox | local server: POSTs the `remote_id IS NULL` set to the team `server_url`, records the returned ids |
| Holding the live pull | local server: holds the SSE socket and the since-cursor catch-up against the team server |
| Local relay surface (`/local/relay/*`) | local server: a small loopback surface the CLI drives (hand the CLI the rows that arrived to apply, take from it the outbox rows to push and the id stamps to record); a transient buffer only |
| Opening or writing `memory.db` | CLI-side storage code only: every read and write, including stamping ids after a push, stays in the CLI; the server holds no handle to any project's `memory.db` |

The relay never trusts a raw SSE payload as data: an SSE frame is a wake-up signal
("something changed, catch up"), and the pulled rows always come from the
since-cursor path keyed on the one cross-store identity both paths agree on (the
UUIDv7 `sync_id`), so no duplicate local row is created.

Rejected, the server opening `memory.db` directly: no precedent (the one existing
cross-store op has the CLI itself open both files sequentially); a machine-global
daemon has no project-path knowledge; keeping `memory.db` single-owner avoids
cross-process SQLite write hardening; and it forecloses nothing about the eventual
store-of-record answer. Durability: the push set (`remote_id IS NULL`) and cursor
(`MAX(remote_id)`) are derived live from `memory.db`, so a relay killed mid-flight is
lossless, since a cold start recomputes both. The buffer is a latency optimisation,
never a source of truth.

**D6. Auto-start scope: interactive `local_first` only.**

| Invocation (`mode = local_first`) | Auto-starts the relay? |
|---|---|
| Interactive (TTY) write or command | yes: may opportunistically start the local server so the outbox drains promptly |
| Non-interactive (CI, scripts, hooks, any non-TTY) | no: the write lands in the outbox and the command exits; it converges on the next interactive or explicit trigger |
| Explicit `inkentry server start` or `inkentry sync` | yes: the user asked for it |
| `INKENTRY_NO_SERVER=1`, or `mode = offline` | never |

The non-blocking guarantee holds regardless: under `local_first` a write commits to
`memory.db` and is durably queued in the outbox whether or not a reconciler is
running. Auto-start scope changes only when the outbox drains, never whether a write
blocks or is lost, so a CI or hook write is instant and durable and converges the
next time the project is touched interactively.

## Store-of-record for teams

Per ADR-004's amendments (not restated here): under `local_first` the team server is
a convergence peer, not authoritative over any member's local reads; under
`cloud_first` it is authoritative for that one invocation only. No member's local
store is second-class, and concurrent writes resolve keep-both. The local server thus
gains a third role, inference and now a `local_first` sync relay, while remaining, as
ADR-004 section 2 requires, never a memory store and never authoritative over
`memory.db`.

## Security implications

- Sync runs as the user, authenticated by the existing bearer; it adds no privilege.
- The relay is the daemon's only outbound surface, so its destination is a
  capability, not a parameter: every destination is resolved from local
  configuration (`RelayPolicy` over `declared_team_targets`), never from a request
  field. This is the whole of what separates the relay from an open egress proxy on
  loopback.
- The relay surface is loopback-only; which local responder the CLI will hand a body
  or bearer to is hardened by ADR-091.
- The detached daemon never opens the OS keychain (enforced by a source-level CI
  scan), so a bearer arrives in the request rather than being resolved by the daemon.
- Full model: [`THREAT-MODEL.md` local relay](../security/THREAT-MODEL.md#local-relay--localrelay-adr-037-p2);
  the v1 server audit covers this surface in
  [`V1-SERVER-AUDIT.md` section 9](../security/V1-SERVER-AUDIT.md).

## Consequences

- This repo is self-contained: every `ADR-037` and `ADR-037 P2` reference in code and
  in the security documents resolves in-repo, with no cross-repo dependency.
- `sync` semantics change: push is text-only and idempotent; the push-seed behaviour
  (re-POST every row, drop `supersedes`) is gone.
- The local schema carries a UUIDv7 identity (ADR-078) as the sync key.
- The local server carries a third role but never becomes a memory store; ADR-004's
  roles framing is extended, not reversed.
- Idle-reaping of a more-eagerly-started server is a follow-up, not part of this
  decision.

## What would falsify this

- The relay opening or writing a project's `memory.db`: a diff that imports the
  storage layer into `crates/inkentry-server/src/relay/` means this record was read
  wrongly.
- A relay destination taken from a request field rather than from `RelayPolicy`.
- The local server becoming authoritative over `memory.db` for any read, in any mode
  other than `cloud_first`'s explicit per-invocation override.
