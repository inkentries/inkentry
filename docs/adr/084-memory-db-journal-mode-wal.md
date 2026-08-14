
# ADR-084: `memory.db` moves to WAL, `synchronous=FULL` unchanged

**Date:** 2026-08-14
**Deciders:** architect (this record); founder (Johan) sign-off via PR review
**Relationship to prior ADRs:** none directly. Touches the storage conventions
CLAUDE.md already states ("Neither store migrates... `memory.db` refuses an
earlier product's store... because its rows are authored") and the multi-store
consistency guarantees `crates/inkentry-cli/tests/security_tests/crash_safety.rs`
pins across `index.db` and `memory.db`. Brings `memory.db`'s journal mode in
line with the precedent `index.db` and `registry.db` already set.

## Context

Diagnosed 2026-08-14 while investigating why some Windows CI tests run 20–60×
slower than their neighbours. The cause had nothing to do with HTTP.

### The asymmetry

Three SQLite stores, one of them different:

| store | pragma on open | |
| --- | --- | --- |
| `index.db` | `journal_mode=WAL; foreign_keys=ON` | `storage/db.rs:62` |
| `registry.db` | `journal_mode=WAL; foreign_keys=ON` | `registry.rs:63` |
| **`memory.db`** | **`foreign_keys = ON` only** | `storage/memory/mod.rs:132` |

No `journal_mode` in the code or in `migrations/memory_001_initial.sql`, so
`memory.db` runs in **rollback-journal (`delete`) mode with `synchronous=FULL`**
— SQLite's default when nothing overrides it. Every write is its own autocommit
transaction: create journal file → write → flush → write DB page → flush →
delete journal file. Per row.

On Linux and macOS that is cheap. On Windows — NTFS plus real
`FlushFileBuffers`, plus Defender scanning each newly created file on a hosted
runner — it costs roughly **140ms per transaction**.

### The evidence

Two tests in the same file, eight positions apart, identical in every other
respect:

| | `single_chunk_push_is_one_request…` | `push_local_stamps_remote_id…` |
| --- | --- | --- |
| mock servers | 1 | 1 |
| `reqwest::Client` constructed | 1 | 1 |
| HTTP requests | 1 | 1 |
| **`memory.db` write transactions** | **100** | **4** |
| Windows | **13.0s** | **0.6s** |

`time ≈ 0.14s × transactions` reproduces every test in the affected set over a
0→43s range. Two independent checks: macOS shows the **identical rank order**
at ~1ms/txn rather than ~140ms/txn, and the model predicts a 68s outlier (440
transactions → ~62s) without having been fitted to it. A local A/B on the same
schema measured `journal_mode=delete` at 244µs/txn against `wal` at 59µs/txn —
4× even on APFS, where fsync does not truly flush.

This refuted the two standing hypotheses, on counts rather than argument:
per-client cost (both tests build exactly one client) and retry backoff
(`remote/retry.rs:91` retries only on 429; these mocks return 500).

This is not a test-suite-only problem: `harvest`, `import`, `reconcile` and
`sync` all write to `memory.db` per row in production, on every platform this
product ships to.

### Why this is a decision, not a one-line fix

`memory.db` holds **authored** data. This codebase deliberately treats it
differently from derived data — refused and pointed at `import` rather than
rebuilt, because nothing can regenerate it (CLAUDE.md, *SQLite + sqlite-vec*).
Changing its durability mode touches that:

- **On-disk contract.** WAL adds `-wal` and `-shm` sidecars beside the store,
  and does not work reliably on network filesystems. `index.db` and
  `registry.db` already carry this trade-off, so it is not a new exposure —
  but `memory.db` is the one store a user cannot simply regenerate if a
  network-filesystem WAL failure ever corrupted it.
- **Recovery semantics.** `crash_safety.rs` pins multi-store consistency
  across `index.db` and `memory.db` (its own comment already groups all three
  stores together as never setting `busy_timeout`, `crash_safety.rs:727`).
  WAL changes what a crash leaves behind — the write-ahead log itself is the
  recovery record, not a journal file rolled back on next open — so its drills
  need to be re-verified against the new mode, not assumed to still hold.
- **The win is unmeasured on the platform that pays it, for the option this
  ADR does not take.** Most of the 140ms should be the journal file
  create/delete, which WAL eliminates — but WAL at `synchronous=FULL` (what
  `index.db` already runs) still fsyncs per commit, so "most" is inference
  from the local A/B, not a Windows measurement of *this exact change*. See
  *Verification* below for how that inference gets checked rather than just
  trusted.

## Decision

**Set `journal_mode=WAL` on `memory.db`, matching `index.db` and
`registry.db` exactly. Leave `synchronous=FULL` unchanged.**

Concretely, `crates/inkentry-core/src/storage/memory/mod.rs`'s `MemoryStore::open`
gains the same pragma line `Database::open` already runs
(`storage/db.rs:62`):

```rust
conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON")?;
```

replacing today's `foreign_keys`-only batch.

### Why this option, and not the other three on the table

1. **WAL, `synchronous=FULL` (chosen).** Consistent with the two other stores
   — a reviewer no longer has to remember which store is the exception.
   Keeps the durability guarantee the authored store has always had: WAL at
   `synchronous=FULL` still fsyncs the WAL file on every commit, so a crash
   loses nothing that a committed transaction promised. The measured 4×
   local win (244µs → 59µs/txn) came from eliminating the journal file's
   create/delete pair, which this captures without touching the fsync
   discipline at all.
2. **WAL + `synchronous=NORMAL`.** A larger measured win is plausible (NORMAL
   skips the fsync between the WAL write and the checkpoint), but it is an
   explicit statement that a crash may lose the last commits of the one store
   that cannot be regenerated. That is a materially different risk posture
   from option 1, deserves its own falsification against real crash
   scenarios, and is not warranted by the evidence in hand — the 140ms
   problem is dominated by journal-file overhead, which option 1 already
   removes. **Not adopted here; may be revisited later as its own decision
   if option 1's measured win turns out insufficient.**
3. **Batch the writers instead** (`add_note` / `set_remote_id`, called per
   item in `push/mod.rs:281` and `memory/sync.rs:65`, wrapped in one
   transaction per push/pull round). This is real and worth doing, but it is
   a narrower, non-durability-mode change — it does not touch
   `journal_mode` or `synchronous`, so per this sweep's own instructions it
   does not need an ADR. **It is not exclusive with option 1** — batching
   cuts transaction *count*, WAL cuts per-transaction *cost* — and is left
   as a separate, narrower follow-up rather than folded into this record.
   Whoever picks it up should note it only helps call sites someone
   remembers to wrap; WAL benefits every writer unconditionally, including
   ones added later.
4. **Leave it, document why.** Rejected: the cost is measured, not
   theorized, it is not confined to CI (production bulk paths pay it too),
   and a same-crate precedent (`index.db`, `registry.db`) already shows the
   fix is cheap and proven safe in this codebase.

## Non-goals

- **Not changing `synchronous`.** `memory.db` keeps `FULL`, exactly as
  `index.db` already does. This ADR does not weaken crash durability for the
  authored store in any way.
- **Not a migration ladder.** `journal_mode` is a per-connection pragma
  (unlike `user_version`), so this needs no schema migration and no version
  bump — every `MemoryStore::open` call sets it, the same way `Database::open`
  already does. A store opened by an older binary before this change reads
  and writes fine; the mode simply takes effect from the next `open` running
  the new pragma.
- **Not addressing network-filesystem WAL limitations.** `index.db` and
  `registry.db` already carry this trade-off; this ADR extends an existing,
  accepted posture to a third store rather than introducing a new one.
- **Not batching the per-row writers.** See option 3 above — deliberately
  left as separate, later work.
- **Not touching `busy_timeout`.** Out of scope; `crash_safety.rs:727`'s
  observation that no store sets it stands unchanged by this ADR.

## Verification (what would falsify this)

A Windows CI run comparing the same two tests named in *The evidence*, with
the pragma changed, is the falsification test: **if per-transaction cost does
not drop substantially, the journal-file hypothesis is wrong and the 140ms is
somewhere else**, and this decision should be revisited rather than declared
done on the strength of the local A/B alone.

## Consequences

- **`-wal` and `-shm` sidecar files appear beside `memory.db`.** Any code or
  tooling that copies/backs up `memory.db` by its single filename (rather than
  via SQLite's own backup API or a full-directory copy) now needs to carry
  the sidecars too, or checkpoint (`PRAGMA wal_checkpoint(TRUNCATE)`) before
  copying. Worth an explicit grep across the codebase and `docs/` during
  implementation for any place that assumes `memory.db` is one file.
- **`crash_safety.rs`'s existing drills need re-verification**, not just a
  green run: the disk-full-during-memory-add test
  (`disk_full_during_memory_add_surfaces_a_clean_error_and_note_is_not_partially_stored`)
  enforces its cap via `INKENTRY_TEST_MAX_PAGE_COUNT`, a per-connection SQLite
  page-count limit; WAL's checkpoint timing differs from rollback-journal's
  immediate write, so confirm the test still triggers `SQLITE_FULL` at the
  same semantic point (a single failed `INSERT` still leaves no partial row)
  rather than merely still passing by coincidence.
- **CLAUDE.md's module map / SQLite section** should be updated once this
  lands, so `memory.db` is no longer implicitly described as the odd one out.
- **Network-filesystem users of `memory.db`** (projects kept on a network
  share) inherit WAL's known unreliability there, same as `index.db` and
  `registry.db` already do — a pre-existing, accepted trade-off in this
  codebase, now applied uniformly rather than to two of three stores.

## Acceptance criteria

1. `storage/memory/mod.rs`'s `MemoryStore::open` sets `journal_mode=WAL`
   alongside `foreign_keys=ON`, matching `Database::open`'s pragma line.
2. `crash_safety.rs`'s multi-store consistency drills (in particular the
   disk-full-during-memory-add test and any drill asserting `memory.db`
   integrity after a crash) pass under the new mode, with the WAL-specific
   caveat above explicitly checked, not assumed.
3. A Windows CI run of the two comparison tests named in *The evidence* shows
   substantially reduced wall time, consistent with the journal-file-overhead
   hypothesis (see *Verification*).
4. CLAUDE.md's storage documentation is updated to reflect `memory.db` now
   matching `index.db`/`registry.db` on journal mode.
5. No change to `synchronous` anywhere in this diff — `FULL` throughout,
   confirmed by review.
