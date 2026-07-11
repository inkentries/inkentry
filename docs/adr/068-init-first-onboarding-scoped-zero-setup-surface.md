## ADR numbering note

ADR numbers are a single global sequence across `spelunk-oss/docs/adr/` and
`cloud-api/docs/adr/`. `068` is free in spelunk-oss (max here is 067) and in
cloud-api (max there is 065) as of 2026-07-11. **Cross-check `cloud-api/docs/adr`
for a `068-*` before merge** — this ADR was authored from the spelunk-oss
worktree, which cannot see concurrent cloud-api additions.

---

# ADR-068: Lead onboarding with `init`; keep a small, honestly-scoped index-free surface

**Date:** 2026-07-11
**Deciders:** founder (Johan) — *pending review*; architect
**Relationship to prior ADRs:** completes the product-direction decision that
[ADR-067](067-fail-closed-no-local-project.md) explicitly deferred to
spelunk-oss^134 ("Broader UX direction is out of scope … a separate product
decision"). Does **not** supersede ADR-067: 067's fail-closed behaviour is the
correctness floor this ADR builds on. Retires the "no setup needed / stored in
git notes — no database" framing in `getting-started.mdx` (marketing-site).

## Context

A UAT "walk-the-store" session walked every command in the opening two sections
of `getting-started.mdx` ("First commands — no setup needed" and "Search and
memory together") against a real, populated, previously-un-`init`'d checkout of
`getlago/lago`. **Zero of eight** invocations behaved as the doc advertised
(spelunk-oss^134 synthesis). The doc's promises are explicit and load-bearing:

- Front matter: *"No API keys or servers required to start."*
- *"First commands — no setup needed … Open a terminal inside any git
  repository. Nothing to configure."*
- *"Memory is stored in git notes — no database, no server, no setup. It travels
  with the repo."*

Grounding each failure in the code (this ADR was written against the tree at
`main`, not from the ticket text):

### What actually runs before `init`

| Doc command | Code path | Real behaviour in an un-`init`'d, populated repo |
|---|---|---|
| `spelunk graph <symbol>` | `cmd/graph.rs:52` → `helpers::open_project_db` → `config::resolve_db` | `resolve_db` (`config.rs:268`) falls back to the **machine-global** `~/.config/spelunk/index.db` when no local `.spelunk/index.db` is found. If a stray global index exists (from indexing any other repo), `graph` silently reads *that* store. Only if no DB opens at all does it fall to `graph_live` (ast-grep `symbol($$$)` call-site match — exact, unranked, call-sites only). Zero results print `No graph edges found for '<symbol>'` with **no guidance** toward `init` or `--live`. |
| `spelunk search "…" --mode text` | `cmd/search.rs:88`,`97` → `resolve_project_and_deps` (`search.rs:384`) | Explicit `text` mode is **not** the auto path. It calls `require_project_db(…, false)` (fail-closed, ADR-067 D1) and then the `db_path.exists()` guard → hard error *"No index found … Run `spelunk index <path>` first."* This is **correct** for an FTS-over-index mode. The doc simply leads with the wrong invocation. |
| `spelunk search "…"` (**auto**, no `--mode`) | `cmd/search.rs:65–84` | Already degrades to `search_live` (ast-grep) when the index is missing or empty. This is the genuine zero-setup search surface, and the doc never shows it. |
| `spelunk memory add` / `list` / `search` | `cmd/memory/mod.rs:377–380` → `require_project_db(&cfg.db_path, false)` | **Already fail-closed on `main`** (ADR-067 D1 shipped): with no `.spelunk/` directory these error *"no spelunk project here. Run 'spelunk init' first"*. The UAT's "silently uses a global un-scoped DB" is **pre-ADR-067** behaviour; that specific leak is closed. What remains untrue is the **doc**: memory's store of record is the local SQLite `memory.db`, not git notes — git-notes is a best-effort, non-fatal write-through gated by `store_in_git_notes` (default `true`, `config.rs:417`; write-through at `add.rs:100–123`). And memory now requires `init` (a `.spelunk/` dir), so "no setup" is false for memory regardless. |
| `spelunk memory search "…"` (default semantic/hybrid) | `cmd/memory/search.rs:44` → `require_server_client` | After `init`, the default (non-`text`) mode still needs a reachable embedder/server; the failure message steers toward a team `server_url`, which is misleading for a solo user who just needs the local server or `--mode text`. |

### The architectural fault line

The code already draws the line the product decision needs:

- **Index-free, global-store-free, working-tree-only** commands: `search "…"`
  (auto→ast-grep), `search --mode ast-grep`, `graph --live`. ADR-067 D1
  deliberately exempts ast-grep because it "touches no index and no global
  store." These are safe to run anywhere and are the *real* zero-setup surface.
- **Index-backed** commands: `search --mode text` (FTS), `graph <symbol>` (code
  graph edges). These cannot produce meaningful results without `spelunk index`.
  `graph` additionally still reads the global store (the ADR-067 residual, also
  tracked on spelunk-oss^147 for `graph`/`chunks`/`explore`).
- **Memory**: correctly `init`-gated by ADR-067. Making memory "safely local-only
  before `init`" would require re-introducing a per-directory store — exactly the
  commingling path ADR-067 just closed. That is architecturally backwards.

So "engineer full zero-setup usage before `init`" (option 1 in the ^134 brief)
would mean: fuzzy/substring code search without an index, a memory store that
works without `init` without leaking across repos, and semantic memory search
without a server. Each fights a deliberate design choice (index-backed ranking,
ADR-067 isolation, server-owned inference). Option 2 (lead with `init`) matches
what the code already enforces and what the `init` "wizard" concept bets on.

## Decision

**Adopt a hybrid that leans on option 2: lead onboarding with `spelunk init`,
and keep a small, explicitly-labelled index-free surface — do not try to make
the full experience work before `init`.**

### D1 — `getting-started.mdx` leads with `init` (marketing-site)

Restructure the opening so **step 1 is `spelunk init`** (populates the local
`.spelunk/index.db` and registers the project). Retire the "no setup needed /
Nothing to configure / stored in git notes — no database" framing. Replace the
front-matter claim with an honest one (e.g. "no API keys and no external servers
— one `spelunk init` and you're running locally"). The `--mode text`, `memory`,
and semantic examples move **after** `init`. This is doc/marketing work
(marketing-site^32/^33), not a spelunk-oss code change.

### D2 — a named, index-free "quick look" surface survives, honestly framed

Keep exactly these working with **no** `init`, documented as a *live structural
scan*, not the full graph/search:

- `spelunk search "<query>"` (auto mode → ast-grep live) and
  `spelunk search --mode ast-grep`.
- `spelunk graph --live <symbol>` (and `graph <symbol>`'s ast-grep fallback).

Constraint: these must touch **only the working tree**, never a global store.
That makes fixing the `graph` global-store residual (spelunk-oss^147) a
prerequisite for `graph`/`graph --live` to be honestly advertised as
zero-setup — otherwise `graph <symbol>` can silently answer from another repo's
stale global index.

### D3 — do not engineer memory or index-backed search to work pre-`init`

Reject the option-1 sub-goals that fight the architecture:

- **No** per-directory / pre-`init` memory store (would reopen the ADR-067
  commingling leak).
- **No** requirement that `--mode text` work without an index (FTS is
  index-backed by definition; auto mode already covers the index-free case).
- **No** semantic memory/code search without a server (inference is
  server-owned per CLAUDE.md / ADR-004).

### D4 — the remaining ticket work is UX-affordance and messaging, not new subsystems

With the direction set, the open tickets collapse from "one coordinated
zero-setup body of work" into small, mostly-messaging fixes plus doc changes
(see the disposition table). Each should say the same thing: *this needs
`spelunk init` (or `--live` for a quick scan)*.

## Per-ticket disposition

| Ticket | Survives leading with `init`? | Disposition |
|---|---|---|
| **^127 / ^128** — `graph` exact-match only, no signal on zero results | **Partly.** Exact match on the *indexed* graph is expected and correct — precision is the point. | Survives, **rescoped**: the real fix is a zero-result affordance — when `graph <symbol>` finds nothing, guide the user to `spelunk init` (for the full graph) or `spelunk graph --live` (structural scan), optionally a did-you-mean. Drop any goal of "fuzzy graph before init." Depends on ^147 (global-store residual) so a zero result isn't silently answered from a stray global index. |
| **^129** — `search --mode text` hard-errors, demands `index` | **No, as a code bug.** The hard error is correct for an explicit index-only mode. | **Mooted as code.** Becomes a doc fix (marketing-site^33): the zero-setup example must use bare `search "…"` (auto→ast-grep), and `--mode text` moves after `init`. No spelunk-oss change. |
| **^130** — ast-grep fallback has no substring/fuzzy | **Yes, but demoted.** | Survives as an **optional** enhancement to the live scan, not a blocker. Prefer a clear "no matches (live structural scan) — run `spelunk init` for full search" hint now; treat fuzzy/substring as a later nice-to-have. |
| **^131 (P0)** — memory silently uses global un-scoped DB | **Code already fixed** by ADR-067 D1 (`memory/mod.rs:377–380` fail-closes on `main`). | **Verify + doc.** Confirm the fail-close shipped (it has, on `main`), then correct the doc's false "stored in git notes — no database, no setup" claim (store of record is `memory.db`; git-notes is opt-in write-through; memory requires `init`). No new architecture. |
| **^132** — memory add/list scoping follow-up | **Yes.** | Folds into ^131's verify + doc pass; ensure `list`/`add` messaging and `status` backend label (ADR-067 D3) are consistent post-`init`. |
| **^133** — `memory search` hard-errors, misleadingly suggests team `server_url` | **Yes.** | Survives as a **messaging** fix, now post-`init`-scoped: when no embedder/server is reachable, the message should point to `spelunk server start` / `spelunk init` / `--mode text` (which works locally with no server), not imply a team server is required. Consider defaulting to text search when no embedder is available. |
| **marketing-site ^32 / ^33** — doc rewrite | **Yes — this is the primary deliverable of the decision.** | Rewrite `getting-started.mdx` per D1: `init` first, honest zero-setup surface (D2) shown as a "quick look," index-backed/memory/semantic examples after `init`. |
| **spelunk-oss ^147** — graph/chunks/explore still read global store | **Yes — now a prerequisite for D2.** | Fixing the residual is required before `graph`/`graph --live` can be advertised as zero-setup (D2's working-tree-only constraint). Elevate from cleanup to enabling work. |

## Non-goals

- **Not** removing or migrating the global `~/.config/spelunk/` store (ADR-067
  already left it in place behind an explicit-only path; unchanged here).
- **Not** adding a `--global` flag (ADR-067 D2 reserved it; still deferred).
- **Not** specifying the `init` "wizard" UX itself — this ADR only establishes
  that leading with `init` is the chosen direction the wizard implements. The
  wizard remains a separate scoping effort.
- **Not** building fuzzy/substring code search (^130) as part of this direction;
  it is demoted to optional.
- **Not** changing `open_memory_backend` selection semantics or the
  inference-vs-storage split (CLAUDE.md / ADR-004).

## Consequences

- **The opening pitch changes.** "Zero setup / no setup needed / stored in git
  notes — no database" is retired as the headline. The honest pitch is: no API
  keys and no external servers; one `spelunk init` gets you a local index,
  local memory, and a code graph, with a small `--live`/ast-grep surface that
  works before `init` for a quick look. This is Johan's product-positioning call
  and is the reason this ADR stops at the decision point for human sign-off.
- **Five tickets shrink.** ^129 becomes a doc fix; ^131/^132 become verify+doc;
  ^127/^128/^133 become small affordance/messaging fixes; ^130 is demoted;
  ^147 is promoted to an enabler. No new subsystem is built.
- **The `init` wizard is de-risked.** Front-loading `init` is exactly what the
  wizard bets on, so this decision and that effort reinforce rather than
  contradict each other.
- **Revisit if:** Johan wants zero-setup to remain the *headline* promise — that
  would reopen option 1 and require re-litigating the ADR-067 isolation model
  and the server-owned-inference split, which this ADR recommends against.

## Security implications

- No new trust boundary or data flow; ADR-067's fail-closed isolation is
  preserved and D3 explicitly refuses to reintroduce a pre-`init`
  per-directory memory store.
- D2's "working-tree-only" constraint on the index-free surface is a security
  property: `graph`/`graph --live` advertised as zero-setup must not read a
  machine-global store, which is why closing the ^147 residual is a prerequisite
  rather than optional cleanup.

## Status

Product-positioning decision (retiring "zero setup" as the headline) — **pending
founder sign-off**. Authored in `refine`; awaits human review in `verify`. Do not
advance the dependent tickets into `implement` on the strength of this ADR being
open. Once approved, record atomically in `spelunk memory --kind decision`.
