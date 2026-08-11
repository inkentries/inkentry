# Example: Multi-session agent workflow

This example shows how an AI agent uses `inkentry` across multiple sessions to implement a feature incrementally, leaving structured context for each subsequent session.

## Session 1: Planning

The agent receives a task: "Add rate limiting to the API."

```bash
# Orient
AGENT=true inkentry memory list --kind question  # no open questions
AGENT=true inkentry memory list --kind handoff --limit 3  # no handoffs yet

# Understand the codebase (search and graph-edges both read the index;
# run `inkentry init` once if this project has none)
AGENT=true inkentry plumbing graph-edges --symbol Router   # trace middleware wiring
AGENT=true inkentry search "HTTP middleware handler" --only-text --format json

# With server: semantic ranking over code and memory in one list
# AGENT=true inkentry search "HTTP middleware handler" --graph --format json
```

The agent writes a plan as a plain markdown checklist (e.g. in `docs/plans/`):

```
- [ ] Research token bucket vs sliding window for this use case
- [ ] Add RateLimiter struct in src/ratelimit/
- [ ] Wire middleware into the router
- [ ] Add per-endpoint configuration support
- [ ] Write unit tests
- [ ] Update API documentation
```

The agent stores a decision and a question:

```bash
inkentry memory add \
  --title "Rate limiting: will use token bucket per IP address" \
  --body "Sliding window is more accurate but token bucket is simpler and sufficient for our traffic patterns (< 1k RPS). Revisit if we see burst abuse." \
  --kind decision --tags ratelimit

inkentry memory add \
  --title "Should rate limits be configurable per endpoint or global only?" \
  --kind question --tags ratelimit,api
```

The agent marks off the first item and writes a handoff:

```bash
inkentry memory add \
  --title "Handoff: rate limiting, session 1 done" \
  --body "Plan in docs/plans/add-rate-limiting.md. Decision: token bucket per IP. Open question stored about per-endpoint config. No code written yet." \
  --kind handoff --tags ratelimit
```

---

## Session 2: Implementation

```bash
# Orient
AGENT=true inkentry memory list --kind handoff --limit 1
# → reads session 1 handoff

AGENT=true inkentry memory list --kind question
# → sees open question about per-endpoint config
# Agent decides: per-endpoint config, stores the answer

inkentry memory add \
  --title "Rate limits will be configurable per endpoint via config struct" \
  --body "Each route registration accepts an optional RateLimitConfig{ rps, burst }. Global default applies if not set." \
  --kind answer --tags ratelimit,api

# Check existing middleware patterns
AGENT=true inkentry search "middleware router registration" --only-text --format json
AGENT=true inkentry plumbing graph-edges --symbol Router   # trace how middleware is wired
```

The agent implements `src/ratelimit/bucket.rs` and wires the middleware.

```bash
# Re-index so search and the call graph see the new code (incremental)
inkentry index .
```

Marks off two more checklist items in the plan file, then:

```bash
inkentry memory add \
  --title "Handoff: rate limiting, session 2 done" \
  --body "token_bucket.rs implemented. Middleware wired in router.rs for /api/v1 routes. Tests not written yet. Per-endpoint config works via RateLimitConfig struct. Next: unit tests and docs." \
  --kind handoff --tags ratelimit
```

---

## Session 3: Tests and documentation

```bash
# Orient from handoff
AGENT=true inkentry memory list --kind handoff --limit 1
AGENT=true inkentry search "rate limiting decisions" --only-memory --limit 5

# Find existing test patterns
AGENT=true inkentry search "unit test tokio test mock" --only-text --format json
AGENT=true inkentry plumbing graph-edges --symbol RateLimiter   # what already calls into it

# With server: loop search + graph-edges + chunks yourself to synthesise
# AGENT=true inkentry search "testing patterns; how middleware components are tested" --graph
```

The agent writes tests, updates docs, marks remaining checklist items complete.

```bash
inkentry index .   # keep the index current
# Mark the remaining checklist items complete in the plan markdown file
```

---

## Key patterns shown

1. **Session start ritual**: read handoff + read open questions
2. **Decision logging**: every non-obvious choice stored with rationale
3. **Question parking**: blockers stored as questions, answered when resolved
4. **Plan as shared state**: a plain markdown checklist tracks progress across sessions
5. **Handoff as context transfer**: structured summary for the next session
