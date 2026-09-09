# ADR-096: A reserved admission lane for interactive embeds, backed by the warm-context pool

**Date:** 2026-09-09
**Deciders:** Founder (Johan); Architect
**Relationship to prior ADRs:** extends the bounded admission gate in front of
the embedder that
[ADR-070](070-init-embed-lifecycle-and-search-warmup-contract.md)'s embed
lifecycle relies on. It is orthogonal to the priority tiers of
[ADR-080](080-structural-summaries-pagerank-tiered-embed-queue-in-place-reembed.md):
those order which chunks an index pass embeds first, this record orders which
requests reach the embedder first. The wire shape of
`POST /v1/projects/{project_id}/index/embed` is unchanged.

## Context

One embed request holds its admission slot for its whole duration. The server
admits a request through a bounded gate of `EMBED_QUEUE_CAPACITY` slots, then
calls the backend once for the whole request and holds the permit until that
call returns. The bulk caller is sized to make that hold long: the CLI's embed
phase grows each batch toward a 240 s target, clamped at the documented ceiling
of 256 chunks, so a healthy index pass deliberately issues requests that occupy
the embedder for minutes at a time.

Interactive work queues behind exactly that. `inkentry search` and
`inkentry memory search` each embed one query string; `inkentry memory add`
embeds one entry. All three are single-text embeds that take 13 to 21 ms on an
idle server. Timed against a genuine `inkentry init` embed pass on Apple Silicon
with Metal, the same single-text embed took 2.59 s, 60.58 s and 154.39 s across
three runs. The request is not slow; it is waiting.

Two properties of the current engine shape the fix.

- The embedder runs a pool of persistent worker contexts, one per worker
  thread, with no shared lock between them. Overlapping embeds land on separate
  warm contexts, and the pool prefers the first idle worker, so serial work
  keeps reusing one hot context while the rest stay cold. Nothing inside the
  engine serializes one request behind another, and a context is reused across
  calls rather than rebuilt per request.
- The pool holds exactly `EMBED_QUEUE_CAPACITY` contexts, one per admission
  slot, so every admitted embed is guaranteed a free context today.

The gate itself, however, is one undifferentiated queue: a single-text request
and a 256-chunk request compete for the same slots, and the gate's only decision
is admit or shed. The route does not separate them either, since the CLI embeds
a query by posting a one-chunk batch to `/index/embed`. So an interactive embed
is shed — returned a `429` — when an index pass has filled the shared slots, and
it competes for those slots on equal terms with requests sized to run for
minutes.

The interactive paths cannot simply wait longer: `memory add` gives its embed a
5 s budget and stores the entry without a vector past it, so a stalled embed
costs semantic rankability until the next reindex.

## Decision

Two changes: one at the admission gate, one to the engine's context pool — how it
is sized, how each lane's contexts live and die, and how its memory is bounded to
the host. Together they ensure an interactive embed is neither shed nor left
waiting behind a bulk index batch.

### 1. Interactive requests get their own reserved admission lane

`EmbedAdmission` gains a second semaphore of `EMBED_INTERACTIVE_CAPACITY` slots,
tried only by interactive requests. The existing lane keeps
`EMBED_QUEUE_CAPACITY` slots and is otherwise unchanged, so the interactive lane
is additional depth rather than a division of what exists: no caller that is
admitted today is shed after this change.

A request's lane is decided two ways, according to what the caller can be
trusted to know.

- **Server-internal callers declare their lane at the call site.** The query
  embeds in `project_search` and `search_notes`, and the entry embed in
  `add_note`, are interactive. The memory batch push and the vectorless-repair
  worker are bulk, and the repair worker's per-row fallback retry stays bulk
  even though each of its calls carries a single text. Intent, not size, is what
  distinguishes a background sweep from a person waiting. The storage embed path
  is shared by an interactive writer (`add_note`) and by bulk writers (batch
  push, repair), so it takes the lane as a parameter from its caller rather than
  inferring one.
- **`/index/embed` classifies by request size**, because the wire carries no
  intent and never will: `chunks.len() == 1` is interactive. This is
  self-describing, needs no wire change, and takes effect for clients that are
  never upgraded. It is safe against misuse in either direction: a one-chunk
  request cannot hold the embedder long enough to matter, so a bulk client whose
  first calibration batch is one chunk lands in the interactive lane at no cost,
  and a client that batches its interactive work has told the truth about its
  own cost.

A full lane sheds exactly as the single gate does today: `429` with
`Retry-After`, returned immediately, never parked and never retried against the
other lane.

### 2. The warm-context pool spans both lanes

The reserved admission lane decides who is let in. What makes an admitted
interactive embed run at once, rather than queue behind a bulk decode, is that a
warm context is free for it.

Today the pool holds `EMBED_QUEUE_CAPACITY` contexts, one per admission slot, so
every admitted embed is guaranteed a free context. Adding a reserved interactive
lane adds admission slots; leaving the pool at `EMBED_QUEUE_CAPACITY` would break
that guarantee, letting an admitted interactive request find every context busy
with bulk work and queue behind a decode. So the pool is sized to the *total*
admission capacity, `EMBED_QUEUE_CAPACITY + EMBED_INTERACTIVE_CAPACITY`,
restoring one warm context per admittable request. Bulk requests occupy at most
`EMBED_QUEUE_CAPACITY` of those contexts, so at least `EMBED_INTERACTIVE_CAPACITY`
are always free for the interactive lane, and an admitted interactive embed finds
an idle warm context without waiting on a bulk request's decode.

The pool keeps its first-idle worker preference within each lane, so serial work
in a lane keeps reusing one hot context rather than spreading across cold ones.
How each lane builds and retires its contexts differs, and that asymmetry —
decided next — is what keeps the reserved capacity from costing memory when it is
not in use.

The pool must stay a set of independent contexts. Collapsing it to a single
context behind a shared lock would serialize every embed and reintroduce
precisely the stall this record removes, and would also return the engine to
building a context per call under contention. Independent per-worker contexts
are load-bearing here, not incidental.

### 3. The two lanes have asymmetric context lifecycles

The interactive and bulk lanes build and retire their contexts differently,
because they serve opposite workloads.

- **The interactive lane keeps one context persistently hot.** It is built on the
  lane's first request and is then exempt from the idle timeout, never evicted for
  quiet. So an interactive embed never pays context-creation latency: the first
  `search` or `memory add` after any lull lands on a context that is already
  built. A context that is never idle-evicted can still wedge or go stale; the
  lane recovers through the pool's existing rebuild-on-decode-failure path, which
  drops and rebuilds a context that fails to decode, rather than through idle
  eviction.
- **The bulk lane builds its contexts lazily and drops them after 30 s of idle.**
  A bulk context is created on the first bulk job a worker receives and released
  30 s after its last one. During an active index pass, batches arrive far faster
  than that, so a bulk context stays hot for the pass and evaporates shortly after
  it ends rather than pinning memory between passes.

When `EMBED_INTERACTIVE_CAPACITY` is greater than one, exactly one interactive
context is the persistently-hot one; the remaining interactive contexts are built
on demand only when interactive requests are genuinely concurrent, and they retire
on the same idle timeout as bulk contexts. Keeping a single interactive context
permanently resident — rather than all of them — bounds the lane's resting cost to
one context regardless of its capacity, while still giving the common case, serial
interactive use, a context that is always warm. Concurrent interactive requests,
which are rare for a single developer, may pay one context build for the second or
third simultaneously in flight; the guarantee is that no interactive request ever
*queues* behind bulk work, not that every simultaneous interactive request skips
warm-up.

### 4. Lane capacity is gated on available memory

`EMBED_INTERACTIVE_CAPACITY` is resolved at load from the machine's available
system memory — it is neither a fixed constant nor left to later tuning. **Below
8 GB available it is 1; at or above 8 GB it is 3**, with contexts materialised on
demand up to that ceiling as described above. `EMBED_QUEUE_CAPACITY` stays 4. The
gate exists because each warm context is a fixed, non-trivial amount of memory,
and that cost is what the budget below bounds against the machine it runs on.

### Memory budget

A separate change fixes the embedder's micro-batch at its largest rung, 8192
tokens, on every machine, so that identical source yields identical vectors
regardless of host. At that rung a warm context costs roughly 2 GB of KV and
compute buffers, and the pool's contexts are the dominant memory the embedder
holds, so the budget is counted in whole contexts.

The pool's high-water mark is `EMBED_QUEUE_CAPACITY + EMBED_INTERACTIVE_CAPACITY`
contexts — one per admittable request. What is actually resident depends on the
workload, and the asymmetric lifecycle is what keeps the ordinary cases far below
that mark:

- **At rest:** one context, ~2 GB — the interactive lane's hot context, once it
  has served a first request. Every bulk context and every on-demand interactive
  context has idle-dropped.
- **A single index pass** (the ordinary bulk workload, whose batches are issued
  serially): the one hot interactive context plus one warm bulk context, ~4 GB. A
  single pass does not materialise four bulk contexts on its own; four is the
  ceiling for four genuinely concurrent bulk callers.
- **Absolute peak:** `(EMBED_QUEUE_CAPACITY + EMBED_INTERACTIVE_CAPACITY) × ~2 GB`,
  reached only when that many bulk and interactive requests are in flight at the
  same instant, and released within the idle timeout once they are not.

The memory gate keeps the peak proportionate to the machine. On a machine that
reports under 8 GB available, `EMBED_INTERACTIVE_CAPACITY` is 1, so the
interactive lane adds a single ~2 GB context on top of the pre-existing bulk pool,
and its resting cost is that one context — not three. The bulk pool reaches its
4 × ~2 GB only under four-way concurrent bulk load; being lazy and idle-dropped at
30 s, it does not hold those contexts between passes, which is what stops an idle
server from pinning 8 GB of bulk contexts. A machine that qualifies for capacity 3
is by definition one the gate measured as having the headroom. And a lazily-built
context that cannot allocate on the device fails that one request rather than the
process, so the peak is a ceiling the engine degrades under, not one it must
always fit.

### Why fairness lives at admission and pool sizing, not inside the engine

Fairness between the two lanes comes from giving each its own warm context, not
from interleaving both on one. The engine holds no lock that spans a request —
each worker owns its context — so there is no contended resource inside it to
schedule fairly: a bulk request and an interactive request simply run on
different contexts. The mechanisms a request-spanning lock would need are
therefore unnecessary. Breaking a bulk request into interruptible slices so a
waiter can take the next turn buys nothing when the waiter already has its own
context, and there is no shared turn to yield. Early cancellation, which slicing
would also serve, is already finer than a slice would be: a worker checks the
abandon flag before every chunk, so an abandoned bulk request stops at the next
chunk boundary.

## Consequences

- An interactive embed is admitted from a reserved lane and runs on a warm
  context that no bulk request occupies. It is no longer shed because an index
  pass filled the shared slots, and no longer waits out a bulk decode once
  admitted.
- **The admission contract gains a documented request class** and widens rather
  than narrows: multi-chunk callers see the behaviour they see today, and a
  caller admitted before this change is admitted after it.
  `docs/architecture/server-api.md` and `docs/openapi.json` describe the
  reserved lane on the `/index/embed` and memory search `429` responses.
- The engine's context pool grows by `EMBED_INTERACTIVE_CAPACITY` contexts at its
  high-water mark — gated to 1 below 8 GB of available memory and 3 at or above
  it, at roughly 2 GB per context. One interactive context stays resident at rest;
  bulk contexts and any on-demand interactive contexts are built lazily and
  dropped after 30 s of idle, so beyond that single resident context the added
  memory is paid only while a bulk pass and interactive work overlap, not between
  passes.
- The 5 s budget on `memory add` stops being the thing that hides this. It
  stays, as the guard for an embedder that is genuinely unavailable rather than
  merely busy.

### Validation the implementation must produce

The decision rests structurally on the engine having no shared embedder lock and
a per-worker warm-context pool; the tuning does not. `EMBED_INTERACTIVE_CAPACITY`
is decided here — gated on available memory — rather than measured, but the
implementation must still confirm the stall is gone by re-running the filing's
repro on this engine: `memory add` and `search` timed during a genuine `init`
embed pass, p50 and max over at least five runs, against the baseline of
2.59 / 60.58 / 154.39 s and a healthy idle single embed of 13 to 21 ms.

That measurement also shows what each half of the decision contributes. The
warm-context pool already lets overlapping embeds run on separate contexts, so it
may on its own remove the multi-second stall whenever an interactive request is
admitted at all; the reserved lane's distinct job is then the narrower one it is
kept for — guaranteeing the interactive request is admitted rather than shed when
a bulk pass has taken every shared slot. The persistently-hot interactive context
adds a third property the repro should confirm: an interactive embed after an idle
spell returns in the idle-server range rather than paying a context build.
