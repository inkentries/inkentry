# ADR-079: Deprecate `inkentry explore` — reasoning belongs to the caller's agent

**Status:** Proposed (awaiting founder review)
**Date:** 2026-08-07
**Deciders:** founder (Johan) — pending; architect (proposing)
**Relationship to prior ADRs:** overtakes the "keep `/explore` as a scoped,
server-owned route" half of [ADR-002](002-server-ai-endpoint-contract.md) §3;
updates the illustrations in [ADR-001](001-scope-boundaries.md) §1,
[ADR-004](004-unified-memory-storage.md), and
[ADR-068](068-zero-setup-onboarding-git-notes-memory-fallback.md), which name
`explore` as a live command. Each affected section carries a forward-pointing
marker to this record.

## Context

`inkentry explore "<question>"` runs an LLM tool-use loop (search → graph →
read_chunk/read_file → answer) against the local index, using whatever chat
model sits behind the server's `INKENTRY_LLM_URL`. It has a matching
server-side route, `POST /v1/projects/{project_id}/explore`.

Two things make this the moment to remove it.

**It contradicts the product's own boundary.** The identity is "inkentry
retrieves context; you reason over it," and the standing non-goal is that
inkentry is "not an agent, not 'a small, worse agent'." Explore *is* a small
agent. For the core customer — a coding agent that already has a stronger model
and a richer toolset — delegating to explore is a downgrade: it hands the
reasoning to a weaker model with a narrower set of tools. This is the same
reasoning that removed the earlier `ask` command; explore is its surviving
sibling.

**It is unpaid carrying cost.** Pre-revenue, an LLM-backed surface (a server
route, its OpenAPI schema, docs, capability advertising, and version-skew
handling) is risk paid for daily whether or not anyone uses it. There is no
confirmed consumer: the "web ask-your-codebase" use case is hypothetical, and
no first-party surface calls the route in a production path.

Two technical facts make the removal clean rather than delicate:

1. **The command and the route are already decoupled.** The CLI's exploration
   loop issues its LLM calls through the generic `POST
   /v1/projects/{id}/llm/complete` primitive (ADR-002 §1), *not* through
   `/v1/projects/{id}/explore`. The `/explore` route is an orphaned, separate
   single-shot reasoning endpoint that no first-party client calls. Removing it
   does not touch the exploration command, and removing the command does not
   touch `/llm/complete`.
2. **Nothing keys on `explore` for capability resolution.** The server's LLM
   availability signal is the `llm.complete` capability, chosen precisely
   because `explore` predates the `/llm/complete` route. LLM routing reads
   `llm.complete` exclusively; `explore` is referenced only for display,
   telemetry, and error-message text. Dropping it is behaviour-preserving for
   routing.

## Decision

1. **Remove the `inkentry explore` command** and **remove the `POST
   /v1/projects/{project_id}/explore` route** (handler, request schemas, router
   registration, and OpenAPI entry).
2. **Replace the workflow with a skill**, documented in `SKILL.md`: the caller's
   own agent runs the search → graph → read → answer loop using inkentry's
   existing primitives (`inkentry search`, `inkentry graph`, `inkentry chunks`,
   and the caller's own file-read tool). A better model runs the loop, nothing
   leaves the machine, and multi-hop capability is preserved.
3. **`memory harvest` becomes the sole LLM-backed feature.** Combined with the
   move to extractive chunk summaries, harvest is the only feature that still
   calls an LLM — and it is defensible on first principles, because it *writes
   durable memory* (the product's identity) rather than producing an ephemeral
   answer. The generic `/llm/complete` primitive stays as the route behind
   harvest.

### Deprecation path

The command is removed behind a **one-release deprecation stub**: a hidden
subcommand that prints a pointer to the explore skill and **exits with code 2**,
carrying no server probe, no LLM call, and no capability behind it. A follow-up
change deletes the stub one release later.

This is chosen over deleting the command outright. Explore was recently promoted
into the top-level `--help` listing, so it is discoverable and may live in users'
and agents' muscle memory and scripts; a bare "unrecognized subcommand" is a
poor exit for a surface we advertised. Exiting non-zero with an actionable
pointer matches the precedent set for other removed surfaces (removed server
flags fail with a message naming the removal), while still keeping the surface
out of `--help` and off every code path.

## What breaks

- `inkentry explore …` no longer runs; for one release it prints a pointer to
  the skill and exits `2`, then is gone entirely.
- `POST /v1/projects/{id}/explore` returns `404`; its request schemas leave the
  OpenAPI document. `/llm/complete` is unchanged.
- An LLM-configured server no longer advertises the `explore` capability in
  `/v1/health`; it advertises `llm.complete` only.
- `inkentry status` and `inkentry check` no longer show an `explore` line.
  `inkentry status --format json` drops the `explore` key from its `usage_7d`
  object — a documented shape change in the status payload.
- No behavioural change for version skew: a newer CLI talking to an older server
  that still lists `explore` in its health capabilities parses it without error
  and, as before, treats that server as not LLM-capable unless it also lists
  `llm.complete`.

## Alternatives considered

- **Keep explore.** Rejected: it is the standing violation of the "you reason
  over it" boundary and an unpaid carrying cost with no confirmed consumer.
- **Delete the command outright, no stub.** Viable and matches how the earlier
  `ask` command was removed. Rejected in favour of the one-release stub only
  because explore is currently `--help`-visible and worth a soft landing.
- **Keep the route, remove only the command.** Rejected: the route has no
  first-party caller, so keeping it is pure carrying cost.
- **Preserve the exploration step-trace as-is.** The old `--verbose` step trace
  doubled as a retrieval-quality signal (which chunks retrieval surfaced, in
  what order). That value does not need the LLM loop and can be captured, if
  wanted, as a deterministic search/graph harness alongside the existing
  retrieval evals; it is not a reason to keep a shipped command. Treated as an
  optional follow-up, out of scope here.

## Consequences

- The exploration loop's in-process `read_file` path-boundary enforcement goes
  away with the command. The skill documents the equivalent norm (read only
  files inside the project; indexed content is already vetted by the indexer),
  and the threat-model rows for the explore-only surface are updated to reflect
  the removed surface. Rows that also cover harvest or `/llm/complete` keep their
  controls.
- ADR-002 §3 kept both `/explore` and `/plan` as scoped server-owned routes.
  `/explore` is removed here; `/plan` was never implemented as a route, so no
  scoped porcelain inference route remains — `/llm/complete` is the single
  generic inference primitive, exactly as ADR-002's boundary test intended.
- The deletion is also a "features we deleted" story: it closes the last place
  the product ran reasoning on its own infrastructure.

## Boundary test

Reaffirms ADR-002's: a feature that needs LLM inference whose prompt and
orchestration can live in the caller routes through `/llm/complete` (or, better,
runs in the caller's own agent as a skill). A scoped server-owned inference route
is added only when multi-turn orchestration or retrieval policy genuinely must
live server-side — which, with explore gone, nothing currently does.
