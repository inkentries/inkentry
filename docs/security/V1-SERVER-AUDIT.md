# v1.0 Server Security Audit Checklist

**Scope:** spelunk-server (crates/spelunk-server after workspace restructure) — new attack surface not covered by the existing CLI security program.  
**Gate:** Must be completed before v1.0 GA. Blocks the v1.0 tag.  
**Date drafted:** 2026-05-17

The CLI threat model (`THREAT-MODEL.md`) remains valid for the CLI crate. This document covers threats introduced by the HTTP server.

---

## 1. Injection scanning middleware

| Check | Status |
|---|---|
| `src/security/injection.rs` is called in POST /v1/projects/{id}/memory before any DB write | ☐ |
| No client header or query parameter can bypass the scan | ☐ |
| 422 response returns `field` and `category` — never the raw regex | ☐ |
| OnceLock pattern compilation verified: no per-request recompilation | ☐ |
| All 8 default patterns have positive and negative unit tests | ☐ |
| Audit log (`tracing::warn!`) fires on every match | ☐ |

## 2. API key authentication

| Check | Status |
|---|---|
| API key stored as BLAKE3 hash — plaintext never persisted | ☐ |
| Key comparison uses constant-time equality (not `==` on strings) | ☐ |
<!-- spelunk-oss^58 (PR #502, draft): ApiKeyAuth now hashes the configured key with BLAKE3 at
     construction and compares per-request tokens via constant_time_eq_32 on the two 32-byte
     digests — the two rows above are implemented in crates/spelunk-server/src/auth.rs. Sign-off
     (✅ + initials/date) still owned by whoever closes oss^66. -->
| `sk-sp-` prefix format validated before any DB lookup | ☐ |
| Revoked/deleted keys rejected immediately (no cache window) | ☐ |
| Key scope enforced: project-scoped key cannot write to other projects | ☐ |

## 3. Multi-tenancy isolation

| Check | Status |
|---|---|
| Every DB query includes an `org_id` filter (no bare table scans) | ☐ |
| RLS enabled and tested: a valid key from org A cannot read org B's entries | ☐ |
| Integration test: cross-org read attempt returns 403, not 404 or 200 | ☐ |

## 4. Input validation

| Check | Status |
|---|---|
| Title field: max 500 characters enforced at route handler | ☐ |
| Body field: max 50 000 characters enforced at route handler | ☐ |
| UUID path params validated (malformed UUID returns 400, not 500) | ☐ |
| All SQL uses parameterised queries — no string concatenation | ☐ |
<!-- spelunk-oss^60 (PR pending, task/engineer-oss60-20260702-1743): `add_note` now enforces
     MAX_TITLE_LEN=500 / MAX_BODY_LEN=50_000 (handlers.rs) plus an embedding-vector-length ==
     configured-dim check, all returning 400 on violation. `project_id` slugs (not UUIDs — project
     ids are human slugs like `usercise/spelunk`, so this is a length/sanity cap, not UUID-format
     validation) are capped at 200 bytes, centralized in `require_project` plus explicit calls in
     the handlers that don't route through it (add_note/index_embed/project_search/explore/
     llm_complete). Also added in this task, beyond this table: a `tower_http` middleware stack
     (TimeoutLayer 30s exempting /memory/stream, RequestBodyLimitLayer 2 MiB, ConcurrencyLimitLayer
     256) and IP-keyed rate limiting on /explore + /llm/complete — see THREAT-MODEL.md's
     "D — Denial of Service" table for the full breakdown, including the ConcurrencyLimitLayer
     known-limitation note on streaming routes. First two rows above ready for ✅ + initials/date
     sign-off by whoever closes oss^66; UUID-path-param row and SQL-parameterisation row are
     unrelated to this task and remain open. -->

## 5. SSE stream

| Check | Status |
|---|---|
| SSE connections require a valid API key on connection open | ☐ |
| Heartbeat tick re-validates key (revoked key disconnected within 60s) | ☐ |
| SSE events scoped to org — no cross-tenant event possible | ☐ |
| Integration test: revoke key, verify SSE connection closes within 60s | ☐ |

## 6. Dependencies

| Check | Status |
|---|---|
| `cargo audit` passes clean for spelunk-server crate | ☐ |
| `cargo deny` passes (licenses + sources) | ☐ |
| No yanked dependencies in Cargo.lock | ☐ |

## 7. Configuration and secrets

| Check | Status |
|---|---|
| Server refuses to start if JWT_SECRET is absent or < 32 bytes | ☐ |
| DATABASE_URL never logged (trace level or above) | ☐ |
| No secrets in default config files or committed .env files | ☐ |
| `.env*` excluded from any server-side file operations | ☐ |

## 8. Error responses

| Check | Status |
|---|---|
| 5xx responses do not leak stack traces or internal paths | ✅ |
| 422 injection responses reveal category, not pattern | ☐ |
| 401/403 responses consistent — cannot distinguish missing key from wrong key | ☐ |

<!-- spelunk-oss^65 (PR #509, merged): AppError::Internal no longer sniffs the error Display text
     for substrings like "mismatch"/"required" — that was the leak: any future error whose
     message happened to contain those words would have reached the client. The one legitimately
     safe case (per-project embedding dimension mismatch) is now a typed DimensionMismatch error
     mapped to a 400 with a fixed safe message (crates/spelunk-server/src/db.rs, lib.rs); every
     other Internal error returns a fixed generic "Internal server error" 500 regardless of its
     underlying text. Same PR also closed two adjacent robustness gaps found alongside this one
     (not strictly in-scope for this checklist row, noted here for traceability): FTS5 MATCH
     queries are now quoted as literal strings (crates/spelunk-core/src/utils/mod.rs
     fts5_quote_literal(), applied in storage/search.rs + storage/memory/search.rs) so punctuation
     in a search term no longer surfaces a raw FTS5 parse error — except an embedded-NUL-byte
     edge case that still leaks one, tracked as a follow-up in spelunk-oss^75; and
     `spelunk index` now applies a uniform MAX_FILE_BYTES size gate before reading any file
     format into memory (crates/spelunk-cli/src/cli/cmd/index/parse_phase.rs), not just the
     tree-sitter branch. Row marked ✅ 2026-07-03 by docs-writer per test-engineer verification
     (task comments on spelunk-oss^65); other two rows in this section are unrelated to this task
     and remain open. -->

---

## Running the checks

```bash
# From spelunk-server crate root (after workspace restructure)
cargo audit
cargo deny check
cargo clippy -p spelunk-server -- -W clippy::all -D warnings

# Integration tests (require running server + postgres)
cargo test -p spelunk-server --test integration

# Cross-tenant isolation test
cargo test -p spelunk-server cross_tenant

# SSE revocation test
cargo test -p spelunk-server sse_key_revocation
```

---

## Sign-off

All checks must be marked ✅ before the v1.0 tag is created. Add initials + date next to each check when complete.
