# Example: Multi-session agent workflow

This example shows how an AI agent uses `spelunk` across multiple sessions to implement a feature incrementally, leaving structured context for each subsequent session.

## Session 1: Planning

The agent receives a task: "Add rate limiting to the API."

```bash
# Orient
AGENT=true spelunk memory list --kind question  # no open questions
AGENT=true spelunk memory list --kind handoff --limit 3  # no handoffs yet

# Understand the codebase
AGENT=true spelunk graph Router --kind calls    # trace middleware wiring
AGENT=true spelunk search "HTTP middleware handler" --mode text --format json

# With server: richer search + AI-generated plan
# AGENT=true spelunk search "HTTP middleware handler" --graph --format json
# AGENT=true spelunk ask "How is the HTTP layer structured? Where would middleware be added?" --json
# spelunk plan create "add rate limiting to the HTTP API layer"
```

The agent writes a plan manually (or generates one via `spelunk plan create` if a chat model is configured):

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
spelunk memory add \
  --title "Rate limiting: will use token bucket per IP address" \
  --body "Sliding window is more accurate but token bucket is simpler and sufficient for our traffic patterns (< 1k RPS). Revisit if we see burst abuse." \
  --kind decision --tags ratelimit

spelunk memory add \
  --title "Should rate limits be configurable per endpoint or global only?" \
  --kind question --tags ratelimit,api
```

The agent marks off the first item and writes a handoff:

```bash
spelunk memory add \
  --title "Handoff: rate limiting, session 1 done" \
  --body "Plan in docs/plans/add-rate-limiting.md. Decision: token bucket per IP. Open question stored about per-endpoint config. No code written yet." \
  --kind handoff --tags ratelimit
```

---

## Session 2: Implementation

```bash
# Orient
AGENT=true spelunk memory list --kind handoff --limit 1
# → reads session 1 handoff

AGENT=true spelunk memory list --kind question
# → sees open question about per-endpoint config
# Agent decides: per-endpoint config, stores the answer

spelunk memory add \
  --title "Rate limits will be configurable per endpoint via config struct" \
  --body "Each route registration accepts an optional RateLimitConfig{ rps, burst }. Global default applies if not set." \
  --kind answer --tags ratelimit,api

# Check existing middleware patterns
AGENT=true spelunk search "middleware router registration" --mode text --format json
AGENT=true spelunk graph Router --kind calls   # trace how middleware is wired
```

The agent implements `src/ratelimit/bucket.rs` and wires the middleware.

```bash
# Re-index if project uses semantic search (skippable otherwise)
spelunk index .
```

Marks off two more checklist items in the plan file, then:

```bash
spelunk memory add \
  --title "Handoff: rate limiting, session 2 done" \
  --body "token_bucket.rs implemented. Middleware wired in router.rs for /api/v1 routes. Tests not written yet. Per-endpoint config works via RateLimitConfig struct. Next: unit tests and docs." \
  --kind handoff --tags ratelimit
```

---

## Session 3: Tests and documentation

```bash
# Orient from handoff
AGENT=true spelunk memory list --kind handoff --limit 1
AGENT=true spelunk memory search "rate limiting decisions" --limit 5

# Find existing test patterns
AGENT=true spelunk search "unit test tokio test mock" --mode text --format json
AGENT=true spelunk graph RateLimiter --kind calls   # find what already calls into it

# With server: ask for a synthesis
# AGENT=true spelunk ask "What testing patterns are used in this codebase? How are middleware components tested?"
```

The agent writes tests, updates docs, marks remaining checklist items complete.

```bash
spelunk index .   # only if project is indexed
spelunk plan status add-rate-limiting-to-the-http-api-layer
# → [##########] 6/6 (100%)
```

---

## Key patterns shown

1. **Session start ritual**: read handoff + read open questions
2. **Decision logging**: every non-obvious choice stored with rationale
3. **Question parking**: blockers stored as questions, answered when resolved
4. **Plan as shared state**: a plain markdown checklist tracks progress across sessions
5. **Handoff as context transfer**: structured summary for the next session
