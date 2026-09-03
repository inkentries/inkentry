# ADR-096: A reserved admission lane for interactive embeds, and sliced bulk embed requests

**Date:** 2026-09-03
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

One embed request holds the embedder for its whole duration. The server admits
a request through a bounded gate of `EMBED_QUEUE_CAPACITY` slots, then calls the
backend exactly once for the whole request and holds the permit until that call
returns. The native backend takes its forward-pass mutex, runs every sub-batch
of `EMBED_BATCH_SIZE`, and releases the mutex only when the last one finishes.
Nothing between the gate and the mutex can interleave other work.

The bulk caller is sized to make that hold long. The CLI's embed phase grows
each batch toward `TARGET_BATCH_SECONDS` (240 s), clamped at the documented
ceiling of 256 chunks, so a healthy index pass deliberately issues requests that
occupy the embedder for minutes at a time.

Interactive work queues behind exactly that. `inkentry search` and
`inkentry memory search` each embed one query string; `inkentry memory add`
embeds one entry. All three are single-text embeds that take 13 to 21 ms on an
idle server. Timed against a genuine `inkentry init` embed pass on Apple Silicon
with Metal, the same single-text embed took 2.59 s, 60.58 s and 154.39 s across
three runs. The request is not slow; it is waiting.

The gate cannot help, because it is one undifferentiated queue: a single-text
request and a 256-chunk request compete for the same slots, and the gate's only
decision is admit or shed. The route does not separate them either, since the
CLI embeds a query by posting a one-chunk batch to `/index/embed`.

Two further facts constrain any fix. The interactive paths cannot simply wait
longer: `memory add` gives its embed a 5 s budget and stores the entry without a
vector past it, so a stalled embed costs semantic rankability until the next
reindex. And the native backend is not the only backend: the llama engine runs a
context per call with no interior mutex, so a mechanism living inside the native
embedder's lock would fix one engine and not the other.

## Decision

Three changes: one at the admission gate, one at the call into the backend, and
one to make the first two hold under load.

### 1. Interactive requests get their own reserved lane

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
  distinguishes a background sweep from a person waiting.
- **`/index/embed` classifies by request size**, because the wire carries no
  intent and never will: `chunks.len() == 1` is interactive. This is
  self-describing, needs no wire change, and takes effect for clients that are
  never upgraded. The classification is safe against misuse in either direction:
  a one-chunk request cannot hold the embedder long enough to matter, so a bulk
  client that lands in the interactive lane (the CLI's first calibration batch
  is one chunk) costs nothing, and a client that batches its interactive work
  has told the truth about its own cost.

The added depth is bounded at a small number of single-text requests. The native
embedder still serializes every forward pass behind its mutex and the
process-wide candle thread cap is unchanged, so this adds queue depth, not
parallelism or threads. A backend that does run passes concurrently remains the
authority on its own concurrency.

### 2. Bulk work is issued to the backend in slices

The backend declares the granularity at which it can be interrupted, through a
new `EmbeddingBackend::embed_slice_size(&self) -> Option<usize>`. `None` is the
default and means the backend does not serialize, so the caller must not slice
it. The native embedder returns `EMBED_BATCH_SIZE`, its own stated preemption
floor: the cancel flag is already polled between sub-batches, and the CPU path's
single batched matmul is deliberately not interruptible below one sub-batch. The
llama engine returns `None`.

Where a slice size is declared, a bulk caller splits its texts into slices of at
most that size and calls the backend once per slice, threading the same cancel
flag through every call so an abandoned request still stops at the next slice
boundary, and assembling results in request order. The request holds its one
admission permit across all of its slices, because it is still one request.

The response stays a single octet-stream of vectors in request order. The
256-chunk ceiling, the `413`, the long embed request timeout and the Metal
device-loss self-heal are all untouched.

### 3. Bulk slices yield to waiting interactive work, for a bounded time

Slicing alone would leave the outcome to the mutex, which does not promise the
waiter wins the next acquisition. So the gate also carries a count of
interactive requests in flight and a notify handle. Before each slice, a bulk
caller observing interactive pressure above zero waits for it to fall to zero or
for `EMBED_BULK_YIELD_MAX` to elapse, whichever comes first, then proceeds
regardless.

The bound is what keeps a stream of interactive writes from starving an index
pass, and it makes the cost explicit and finite: a bulk pass of `n` texts is
delayed by at most `ceil(n / slice) * EMBED_BULK_YIELD_MAX` beyond its own
compute time, whatever the interactive load.

The gate keeps its present shed behaviour throughout. A full lane returns `429`
with `Retry-After` immediately and never parks.

## Consequences

- A single-text embed waits at most one bulk slice rather than one bulk request.
  At the documented maximum batch that is 8 chunks instead of 256, against a
  bulk caller that sizes its batches to run for minutes.
- **The admission contract gains a documented request class.** A one-chunk
  embed is admitted from a reserved lane and so is no longer shed because an
  index pass filled the shared one. Multi-chunk callers see the behaviour they
  see today. `docs/architecture/server-api.md` and `docs/openapi.json` describe
  the lane on the `/index/embed` and memory search `429` responses.
- **`EmbeddingBackend` gains one additive method** whose default makes every
  existing implementation correct without edits.
- An index pass runs slower under interactive load, by the bounded amount above.
  Its sequences are also length-sorted within a slice instead of across the
  whole request: on the native embedder's Metal path chunks are forwarded one at
  a time, so that costs nothing there, while the CPU batched path can see extra
  padding waste within a slice.
- The 5 s budget on `memory add` stops being the thing that hides this. It
  stays, as the guard for an embedder that is genuinely unavailable rather than
  merely busy.
