# ADR-094: Pushed vectors must be near unit length

**Date:** 2026-09-02
**Deciders:** Founder (Johan); Architect
**Relationship to prior ADRs:** completes the pushed-vector contract alignment
that [ADR-076](076-memory-wire-contract-ownership.md) called for between the
self-hosted server and the hosted peer. It does not change the wire field
names (`vector`, `vector_model`, `vector_precision`) or the
`accepts_pushed_vectors` capability.

## Context

Both memory write routes on the self-hosted server (`POST
/v1/projects/{project_id}/memory` and `.../memory/batch`) accept a
client-computed `vector` in place of server-side embedding. A pushed vector is
checked for model tag, precision (`fp32`), dimension and finite components,
and stored verbatim. The hosted peer runs the same checks plus one more: the
vector's L2 magnitude must fall inside `[0.5, 1.5]`. The self-hosted server
skipped that check when the rest of the contract was aligned, because adopting
it rejects requests the server accepts today.

Whether the check matters depends on the distance metric the index uses. The
server's `note_embeddings` table is a sqlite-vec `vec0` table declared with a
plain `FLOAT[dim]` column, so nearest-neighbour search and conflict detection
both rank by Euclidean distance. Euclidean ranking agrees with cosine ranking
only when every stored vector has unit length. The native embedder guarantees
that for its own output, and the CLI pushes that output unchanged. Nothing
guarantees it for a vector some other client computed.

A vector of the wrong magnitude therefore does not merely look odd: it
corrupts ranking silently. A correctly oriented vector of length 3 ranks below
an unrelated unit vector. A vector of length 0.1 sits closer to every query
than most genuine matches, and the conflict detector (which thresholds the
same distance) flags it against entries it has nothing to do with. Nothing
downstream can tell such a vector from a good one, so the damage is permanent
until the entry is re-embedded.

## Decision

Adopt the hosted peer's magnitude window on both memory write routes.

- After the existing model, precision, dimension and finite checks, compute
  the vector's L2 norm. A norm outside the inclusive range `[0.5, 1.5]` is
  refused.
- The refusal is `400` with the error body every other pushed-vector check on
  this server returns, and the message
  `pushed vector L2 magnitude {norm:.4} outside expected [0.5, 1.5]`
  (norm printed to four decimals). On the batch route it carries the same
  per-entry prefix as the other up-front validation errors and rejects the
  whole batch with nothing stored, matching how the other pushed-vector
  checks behave there.
- The window is deliberately wide. Unit vectors leave the embedder with a
  norm of `1.0` up to floating-point error; the window exists to catch a
  vector that was never normalised, or a zero vector, not to police the last
  digits.
- The server refuses rather than normalising on the caller's behalf. The
  pushed-vector contract is "store exactly what was checked, or refuse":
  silently rescaling would hide the mismatch (a wrong-magnitude vector is
  usually a sign the caller did not produce it the way it claims) and would
  put the two peers back on different behaviour.

## Consequences

- The two server peers now validate a pushed vector identically: same checks,
  same window, same message.
- **Breaking for third-party clients** that push vectors they did not
  L2-normalise. Such clients were already getting degraded retrieval on every
  entry they pushed; they now get a `400` naming the norm. The fix on their
  side is to divide the vector by its L2 norm before pushing, or to omit
  `vector` and let the server embed. The CLI is unaffected: every vector it
  pushes comes from the native embedder, which normalises its output.
- `docs/openapi.json` and the write-route error descriptions gain the new
  `400` case, and the release notes carry a `BREAKING` entry so the newly
  rejected callers can find the reason.
