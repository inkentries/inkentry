# v1.0 Server Security Audit Checklist

**Scope:** `inkentry-server` (`crates/inkentry-server/`), the HTTP attack surface not covered by the CLI security program.  
**Gate:** Must be completed before v1.0 GA. Blocks the v1.0 tag.  
**Date drafted:** 2026-05-17  
**Retargeted:** 2026-07-03 to the OSS server as-built (single-trust-domain tenancy per [ADR-056](../adr/056-oss-server-tenancy-model.md)).  
**Amended:** 2026-08-13 — §9 added for the local relay surface (`/local/relay/*`, ADR-037 P2), which §§1–8 never covered; §3's bind guidance corrected against ADR-066 and the code; §4's SQL claim replaced with a site-by-site inventory.

The CLI threat model ([`THREAT-MODEL.md`](THREAT-MODEL.md)) remains valid for the CLI crate. This document covers threats introduced by the HTTP server.

**On this retarget.** The original draft was written against a cloud-shaped server
(Postgres, `org_id` row-level security, `JWT_SECRET`, `sk-ink-` prefixed keys). The
OSS `inkentry-server` is a single-file SQLite service with one shared bearer key and
no identity, org, or role model. Items that assume the cloud shape are relabelled
below as **N/A (cloud-only)** or **N/A by design (ADR-056)**; they are not
unmet requirements. Boxes are ticked only where the sibling fix has merged and the
code was read in this tree to confirm it; each such row cites the file that
implements it. Applicable-but-unmet boxes stay unchecked with the owning task named.

**Legend:** ☑ done (evidence cited) · ☐ applicable, not yet satisfied · N/A relabelled item (with reason).

---

## 1. Injection scanning

The module is `crates/inkentry-server/src/security.rs` (`scan_for_injection`), not the
originally-drafted `src/security/injection.rs` path. It carries 12 patterns, not 8.

| Check | Status |
|---|---|
| `security::scan_for_injection` is called in `POST /v1/projects/{id}/memory` before any DB write | ☑ `handlers.rs::add_note` scans `title`+`body` and returns 422 before the insert |
| No client header or query parameter can bypass the scan | ☑ the scan runs unconditionally inside the handler on the parsed `title`/`body`; there is no bypass field |
| 422 response returns `field` and `category`, never the raw regex | ☑ `handlers.rs` returns `{field, category, message}`; `security.rs` exposes only the `category` name, never the pattern |
| OnceLock pattern compilation verified: no per-request recompilation | ☑ `security.rs::patterns()` compiles once via `OnceLock<Vec<Pattern>>` |
| All default patterns have positive and negative unit tests | ☑ `security.rs` tests cover every pattern positively plus two clean-input negatives |
| Audit log (`tracing::warn!`) fires on every match | ☑ `handlers.rs::add_note` emits a `tracing::warn!` on a match recording the project slug, field, category, and title/body lengths, and never echoes the matched text |

## 2. Bearer-key authentication

The OSS server authenticates with **one shared bearer key** (`ApiKeyAuth`,
`crates/inkentry-server/src/auth.rs`). There is no per-key database record, no key
prefix format, and no per-project key scope. The shared key is the tenancy
boundary ([ADR-056](../adr/056-oss-server-tenancy-model.md)). The rows that assume a
keys-table / scoped-key model are relabelled accordingly.

| Check | Status |
|---|---|
| Configured key never held or compared as plaintext | ☑ `auth.rs::ApiKeyAuth::new` hashes the key with BLAKE3 into a 32-byte digest at construction; the plaintext is not retained |
| Key comparison uses constant-time equality (not `==` on strings) | ☑ `auth.rs` hashes the provided token and compares digests with `constant_time_eq::constant_time_eq_32` |
| `sk-ink-` prefix format validated before any DB lookup | N/A (cloud-only). The OSS server has no `sk-ink-` prefix and no per-key DB lookup; the key is opaque and matched by digest. |
| Revoked/deleted keys rejected immediately (no cache window) | N/A (cloud-only). There is no key store to revoke from; a single shared key is rotated by restarting the server with a new value (ADR-056). |
| Key scope enforced: project-scoped key cannot write to other projects | N/A by design (ADR-056). The shared key grants full access to every project on the instance; isolation is by running separate instances. |

## 3. Tenancy model (single trust domain)

Reframed to the OSS server's ratified model ([ADR-056](../adr/056-oss-server-tenancy-model.md)):
a server instance is a **single trust domain**, and its shared key is the boundary.
There is no `org_id`, no row-level security, and no cross-tenant isolation *within*
one instance. That is intended, not a gap. Projects on one instance are addressed
by a `project_id` slug in the path, which is a routing key, not a security boundary.
Isolation between teams is achieved by running **separate instances**, each with its
own key and database. The rows below are therefore N/A by design.

| Check | Status |
|---|---|
| Every DB query includes an `org_id` filter (no bare table scans) | N/A by design (ADR-056). No `org_id` exists; queries scope by `project_id` slug, which is an addressing key, not an isolation control. |
| RLS enabled and tested: a valid key from org A cannot read org B's entries | N/A by design (ADR-056). SQLite has no RLS and the model has no orgs; one key = one trust domain over all projects. |
| Integration test: cross-org read attempt returns 403, not 404 or 200 | N/A by design (ADR-056). Cross-project access with the shared key is intended behaviour; there is no cross-org boundary to test. |

**Operator guardrail (applicable, verify before GA):** the server must emit a
startup notice on a keyed, non-loopback bind stating that every keyholder is a full
administrator of all projects and that separate instances are the way to isolate.
Status: ☑ implemented in `main.rs::should_warn_single_trust_domain` /
`warn_single_trust_domain`, which fire the ADR-056 notice on a keyed non-loopback bind.

**Transport guardrail (applicable, ☑ implemented):** the shared key is a bearer
credential that must not travel in cleartext (ADR-056). `main.rs::check_bind_safety`
therefore refuses **unconditionally** to bind a non-loopback address over plaintext
HTTP, covering both the keyless case (an open, unauthenticated server) and the keyed
case (the bearer key would cross the network in the clear). The refusal names the
interface/port, and there is no override for it.

What is refused unconditionally is **plaintext off-host**, not off-host itself. A
shared server has a supported non-loopback posture: bind off-host over HTTPS the
server terminates **in-process** (`--tls-cert`/`--tls-key`) with an API key set —
the path ADR-066 §4 ratified, which `check_bind_safety` implements as its final
`Ok(())`. (An earlier revision of this section stated that loopback was the only
supported posture for a shared server, and cited a unit test
`non_loopback_with_key_plaintext_is_refused_unconditionally` that does not exist.
Both were wrong: they pre-dated ADR-066 and contradicted the bind table in
[`THREAT-MODEL.md`](THREAT-MODEL.md#mode-b--inkentry-server), which has been correct
throughout.) The four-row decision is:

| Bind | TLS | Key | `check_bind_safety` |
|---|---|---|---|
| loopback | any | any | allow — `loopback_is_allowed_for_every_combination` |
| non-loopback | no | any | refuse — `non_loopback_without_tls_is_refused` |
| non-loopback | yes | no | refuse — `non_loopback_tls_without_key_is_refused` |
| non-loopback | yes | yes | allow — `non_loopback_tls_with_key_is_allowed` |

All four unit tests live in `crates/inkentry-server/src/main.rs`.

## 4. Input validation

Path params in the OSS server are **project slugs** (e.g. `usercise/spelunk`), not
UUIDs, so the "malformed UUID" row is reframed as a slug length/sanity cap.

| Check | Status |
|---|---|
| Title field: max 500 characters enforced at route handler | ☑ `handlers.rs` `MAX_TITLE_LEN = 500`, returns 400 on violation |
| Body field: max 50 000 characters enforced at route handler | ☑ `handlers.rs` `MAX_BODY_LEN = 50_000`, returns 400 on violation |
| Path param (project slug) validated; an over-long slug returns 400, not 500 | ☑ `handlers.rs` `MAX_SLUG_LEN = 200`, enforced in `require_project` and the handlers that bypass it (add_note / index_embed / project_search / llm_complete) |
| Every value that can originate with a caller is bound as a parameter; nothing caller-derived is interpolated into SQL text | ☑ verified site by site — see the SQL-construction inventory below |

### SQL-construction inventory (`crates/inkentry-server/src/db.rs`)

An earlier revision of the row above read "no `format!`/concatenation into SQL across
`crates/inkentry-server/src/`". That was false as written — `db.rs` builds statement
text with `format!` in eight places — and an absolute claim that is false is worse
than a narrow one that is true, because it is the claim a future reviewer relies on
to skip the check. The precise claim, verified in this tree, is the one in the table:
**no caller-derived value is ever interpolated; only compile-time constants,
programmatically generated placeholder tokens, and server-clamped integers are.**
Every site:

| Site (`db.rs`) | Interpolated | Why it is safe |
|---|---|---|
| `open`/migrate: `CREATE VIRTUAL TABLE … FLOAT[{dim}]` | `self.embedding_dim` (`usize`) | Server startup configuration, not a request field |
| `find_by_remote_ids` | `{placeholders}` — a `?,?,?` token list | Generated from `remote_ids.len()` alone; every id is bound. Length is bounded by `MAX_BATCH_ENTRIES` (200) at the handler, far under SQLite's bind-parameter limit |
| `get_note` | `NOTE_COLUMNS`, `NOTE_SOURCE` | `const &str` in the same file; `note_id`/`project_id` bound as `?1`/`?2` |
| `list_notes` (×2 branches) | `NOTE_COLUMNS`, `NOTE_SOURCE`, `{status_clause}`, `{limit}` | `status_clause` is one of two string literals chosen by a `bool`; `limit` is `usize` after `.min(500)`. The caller-supplied `kind` filter is bound as `?2` |
| `search_notes` | `NOTE_COLUMNS`, `{limit}` | `limit` is `usize` after `.min(100)`; the query vector is bound as a blob |
| `search_notes_for_conflicts` | `{search_limit}`, `{limit}` | Both derived from `limit.min(50)` |
| `notes_since` | `NOTE_COLUMNS`, `NOTE_SOURCE` | Constants only; `project_id`/`since_secs`/`limit` all bound |

The `{limit}` interpolations are `usize` values that have already passed through a
server-side clamp, so they cannot carry text at all — the type, not the clamp, is
what rules out injection; the clamp bounds result size. (`notes_since` shows the same
value bound as `?3` instead, so the interpolated ones are a local inconsistency, not
a necessity.) Two properties are load-bearing and worth a reviewer's attention on any
future edit: **a clamp must stay upstream of every `{limit}`**, and **the
placeholder-token pattern in `find_by_remote_ids` must keep the values on the bind
path** — the same trap documented for the CLI-side storage layer in
[`in-clause-parameterisation.md`](in-clause-parameterisation.md).

No SQL text is constructed anywhere else in `crates/inkentry-server/src/`; `db.rs`
is the only module that talks to SQLite. The previous row also cited
`fts5_quote_literal` as evidence here, which was misattributed: the server issues no
FTS5 `MATCH` query at all (its only `MATCH` is sqlite-vec's `embedding MATCH ?1`,
with the vector bound). `fts5_quote_literal` lives in `inkentry-core` and guards the
CLI-side stores — correct, but not evidence about this crate.

Beyond this table, the input-validation hardening also added a `tower_http` middleware stack (see §DoS in
[`THREAT-MODEL.md`](THREAT-MODEL.md#d--denial-of-service)): `RequestBodyLimitLayer`
(2 MiB), `TimeoutLayer` (30s, exempting `/memory/stream`), `ConcurrencyLimitLayer`
(256), plus peer-address-keyed rate limiting on `/llm/complete`, and an
embedding-vector-length check against the configured dim.

**`/index/embed` timeout carve-out (PR #513 field-failure
follow-up):** the blanket 30s `TimeoutLayer` above made `/index/embed` unusable — a
legitimate calibrated batch (or even a single oversized chunk on slow/CPU-only
hardware) genuinely needs minutes, and was being killed at 30s regardless of what
the CLI's own client-side timeout allowed. Fixed by giving `/index/embed` its own
long-budget timeout (`EMBED_REQUEST_TIMEOUT`, 1800s, matching the CLI's
`MAX_REQUEST_TIMEOUT` ceiling) instead of `REQUEST_TIMEOUT` — same carve-out
pattern as the `/memory/stream` exemption above, not a blanket removal:
`/index/embed` keeps the same `auth_middleware` + `ConcurrencyLimitLayer` +
`RequestBodyLimitLayer` (2 MiB) + its own `MAX_EMBED_BATCH` (256 chunks) handler
cap, so the DoS surface stays bounded (see `THREAT-MODEL.md`'s updated D-table
row). `/v1/health` now also carries a `limits` object
(`embed_request_timeout_secs`, `max_batch_chunks`, `embedder_token_cap`) so a
client can detect and adapt to a server that pre-dates this fix (absent `limits`
⇒ assume the old 30s/no-exemption profile) instead of assuming its own
calibration always fits whatever server it happens to be talking to.

## 5. SSE stream

The OSS server has one shared key with no per-key revocation and no orgs (ADR-056),
so the revocation-window and cross-tenant rows do not apply.

| Check | Status |
|---|---|
| SSE connection requires a valid key on connection open | ☑ the `/v1/projects/{id}/memory/stream` route is mounted under `auth_middleware` (`lib.rs`), so the key is checked before the stream opens |
| Heartbeat tick re-validates key (revoked key disconnected within 60s) | N/A by design (ADR-056). There is no per-key revocation store; a single shared key is rotated by restarting the server. The keep-alive tick is a transport ping, not a re-auth. |
| SSE events scoped to org (no cross-tenant event possible) | N/A by design (ADR-056). No orgs; the stream is scoped to a `project_id` slug within the single trust domain. |
| Integration test: revoke key, verify SSE connection closes within 60s | N/A by design (ADR-056). No revocation mechanism to test against. |

## 6. Dependencies

Verified against CI (`.github/workflows/security.yml`), which runs on every push
and PR to `main` plus a weekly schedule.

| Check | Status |
|---|---|
| `cargo audit` passes clean (workspace, includes inkentry-server) | ☑ `security.yml` runs `cargo audit`; it fails the job on any unignored advisory |
| `cargo deny` passes (advisories + licenses + bans + sources) | ☑ `security.yml` runs `cargo deny check advisories licenses bans`; `deny.toml` also defines `[sources]` |
| No yanked dependencies in Cargo.lock | ☑ `Cargo.lock` is committed and `cargo audit` reports yanked crates by default, gating the same CI job |

## 7. Configuration and secrets

The OSS server has no `JWT_SECRET` and no `DATABASE_URL`; it is a single-file SQLite
service authenticated by one bearer key. The JWT/database rows are relabelled.

| Check | Status |
|---|---|
| Server refuses to start if `JWT_SECRET` is absent or < 32 bytes | N/A (cloud-only). The OSS server has no JWT; auth is the shared `INKENTRY_SERVER_KEY`. The applicable startup guard is `main.rs::check_bind_safety`, ☑ implemented: it refuses a non-loopback plaintext bind **unconditionally** in both the keyless case (open server) and the keyed case (bearer key in cleartext), naming the interface. Neither refusal has an opt-out. |
| `DATABASE_URL` never logged | N/A (cloud-only). No `DATABASE_URL`; the DB is a local SQLite file path. The applicable rule, that the bearer key is never logged, holds: ☑ `auth.rs` never logs the key or its hash. |
| No secrets in default config files or committed `.env` files | ☑ verified: no committed `.env` and no secrets in the server's default config; `INKENTRY_SERVER_KEY` is supplied by the operator at runtime |
| `.env*` excluded from any server-side file operations | N/A. The server does not walk the filesystem or index files; only the CLI indexer reads project trees (where `.env*` exclusion applies, and is documented in the CLI program). |

## 8. Error responses

| Check | Status |
|---|---|
| 5xx responses do not leak stack traces or internal paths | ☑ `AppError::Internal` returns a fixed generic "Internal server error" 500 regardless of the underlying error text; the one safe case (embedding dim mismatch) is a typed 400 with a fixed message (`lib.rs`, `db.rs`) (PR #509) |
| 422 injection responses reveal category, not pattern | ☑ `handlers.rs` returns `{field, category, message}`; the raw regex is never exposed (see §1) |
| 401 responses consistent (cannot distinguish a missing key from a wrong key) | ☑ `auth.rs` returns the same `AuthError("Unauthorized")` mapped to 401 for both a missing `Authorization` header and a wrong bearer token; there is no 403 path (single shared key), so the missing-vs-wrong distinction does not leak |

<!-- Evidence note (PR #509, merged): AppError::Internal no longer sniffs the error
     Display text for substrings like "mismatch"/"required"; that was the leak. The one
     legitimately safe case (per-project embedding dimension mismatch) is now a typed
     DimensionMismatch error mapped to a 400 with a fixed safe message
     (crates/inkentry-server/src/db.rs, lib.rs); every other Internal error returns a fixed generic
     "Internal server error" 500. The same PR also quoted FTS5 MATCH terms as literals
     (crates/inkentry-core/src/utils/mod.rs fts5_quote_literal, applied in storage/search.rs +
     storage/memory/search.rs), with an embedded-NUL-byte edge case tracked as a separate
     follow-up, and added a uniform MAX_FILE_BYTES gate in
     crates/inkentry-cli/src/cli/cmd/index/parse_phase.rs. -->

## 9. Local relay surface (`/local/relay/*`, ADR-037 P2)

Added to this checklist in August 2026. The surface pre-dates the entry: it was not
covered by §§1–8, which were written against the `/v1` team-hosting routes, and it
had no threat-model entry either. Full model:
[`THREAT-MODEL.md` → Local relay](THREAT-MODEL.md#local-relay--localrelay-adr-037-p2).
It is audited separately because it is the one surface on this binary that makes it
open **outbound** connections, so the §§1–8 controls (which all bound what a caller
may put *in*) do not speak to it.

| Check | Status |
|---|---|
| Not served on a bind another machine can reach | ☑ `RelayRegistry::for_bind` returns a disabled registry off loopback, and `lib.rs::router` does not mount the three routes when the registry is disabled — refusal *and* absence, not a per-handler check. Tests: `the_relay_is_disabled_on_a_non_loopback_bind`, `a_disabled_registry_refuses_every_push` |
| The outbound destination cannot be supplied by a request | ☑ every destination is resolved by `relay::RelayPolicy` from `inkentry_core::config::declared_team_targets` (env pair, `.inkentry/config.toml` above the daemon's cwd, local registry projects). `RelayPolicy::from_fn`'s source closure takes no arguments, so no policy can be built that a request can reach. Tests: `a_server_url_no_local_config_declares_is_refused`, `a_declared_server_with_an_undeclared_project_is_refused` |
| A declared target still cannot be plaintext off-host | ☑ `RelayRegistry::resolve` re-runs `validate_transport_url` before any session exists. Test: `a_declared_but_plaintext_non_loopback_target_is_refused` |
| Refusals and errors leak nothing about the remote host | ☑ `last_error` is the fixed `REMOTE_HOP_FAILED` string; the `reqwest` detail (refused vs timed-out vs TLS-failed, per host/port) goes only to the daemon log, so the surface is not a network-probe oracle. Test: `last_error_never_carries_the_remote_error` |
| Registry and buffers are bounded | ☑ `MAX_RELAY_SESSIONS` (32), `MAX_BUFFERED_ITEMS_PER_SESSION` (10 000), and an SSE receive-buffer cap. Tests: `the_registry_is_bounded_even_when_local_config_declares_more`, `oversized_sse_frame_without_terminator_errors_instead_of_growing_forever` |
| A session cannot outlive its use | ☑ `SESSION_IDLE_TIMEOUT` (30 min with no CLI call) ends the pull loop and drops the session; background traffic deliberately does not refresh the clock. Tests: `a_sessions_pull_loop_terminates_once_no_cli_is_using_it`, `a_session_in_use_is_not_retired` |
| Idle daemon opens nothing | ☑ a session is only ever created by `push`; `poll`/`ack` never create one. Test: `empty_registry_makes_no_outbound_calls_and_starts_no_sessions` |
| Pulled entries cannot cross projects on one team server | ☑ sessions are keyed on (server, project). Test: `pulled_rows_never_leak_across_projects_on_the_same_team_server` |
| `poll`/`ack` restricted to the calling project | ☑ Both look a session up by (server, project) without consulting `RelayPolicy`, so any local process naming a declared pair — the pair is in the committed `.inkentry/config.toml` — reads that session's buffered entry bodies or retires them. Bounded by the surface being loopback-only and by the pair having to be declared; not closed. Recorded as residual 1 in the threat model |
| A live session's bearer cannot be overwritten or borrowed by another local process | ☑ `set_bearer` takes the bearer from the request (the detached daemon must never open the OS keychain — §7 of this document and the corresponding threat-model row), so a local caller can replace it and stall that session's sync, and a `push` omitting it rides the resident one. Residuals 2 and 3 in the threat model |
| Published in `docs/openapi.json` | N/A by decision. Deliberately unpublished: the spec describes the team-hosting role for clients pointing at a `server_url`, and a non-loopback server never mounts these routes. Reasoning recorded in the threat model's "Why this surface is not in `docs/openapi.json`" |
| Covered by the egress-containment harness | N/A — not coverable there. `crates/inkentry-cli/tests/egress_containment.rs` wraps the **CLI subprocess**, so it cannot observe daemon-side egress. The local-tier default is unaffected (no declared target ⇒ no session ⇒ no outbound call), but the harness must not be cited as covering this surface |

The two ☐ rows are **not** sign-off blockers under the same reasoning as §3's
tenancy rows: they are properties of the deliberate keyless loopback posture
([ADR-056](../adr/056-oss-server-tenancy-model.md)), reachable only by a process
already running as the local user. They are listed unchecked rather than relabelled
N/A because, unlike the tenancy rows, they are not purely intra-machine: they let a
local process act against the **team** server. Closing them needs a local caller
identity the current posture does not provide, which is a post-v1.0 ADR.

---

## Running the checks

```bash
# From the workspace root. Export INKENTRY_SECRET_STORE=file on macOS to avoid
# Keychain prompts during tests.
cargo audit
cargo deny check advisories licenses bans
cargo clippy -p inkentry-server -- -W clippy::all -D warnings

# Server unit + handler tests (SQLite; no external services required)
INKENTRY_SECRET_STORE=file cargo test -p inkentry-server

# Auth, injection-scan, input-cap, and error-mapping tests live in-crate:
#   auth.rs (constant-time key compare), security.rs (injection patterns),
#   handlers.rs (title/body/slug caps, 422 shape, SSE past-timeout),
#   main.rs (§3 bind guard), relay/{mod,policy}.rs + relay/tests.rs (§9).
```

---

## Sign-off

Every **applicable** row must be ☑ (with cited evidence) before the v1.0 tag is
created, **or** carry an explicit, reasoned acceptance in this section. **N/A** rows
carry no obligation; they record a cloud-only item or an ADR-056 by-design decision,
not an outstanding task. A ☐ row is never silently carried past the tag: either it
is ticked with evidence, or the sign-off names it and says why the release proceeds
anyway.

**State at retarget (2026-07-03):** the only applicable row not yet satisfied was
the §1 injection audit-log (`tracing::warn!` on a match); it is now implemented in
`handlers.rs::add_note` and ticked above.

**State at amendment (2026-08-13):** §§1–8 remain fully ☑ with evidence cited. §9
introduces two ☐ rows on the local relay (`poll`/`ack` project scoping, and bearer
ownership on a live session). Both are **accepted, not outstanding work**: they are
consequences of the keyless loopback posture and of the daemon deliberately never
opening the OS keychain, reachable only by a process already running as the local
user, and closing either requires a local caller identity that does not exist yet.
They are recorded as ☐ rather than N/A because they are genuine residual capability,
not by-design intent — a reviewer should be able to see them and disagree. Founder
sign-off must therefore be an explicit acceptance of §9's two open rows, not only a
tick of §§1–8.

**Founder sign-off — accepted for v1.0 (2026-08-21, Johan).** §§1–8 are ☑ with
evidence cited, re-verified against this tree. I explicitly accept §9's two open rows
for the v1.0 tag: Residual 1 (`poll`/`ack` are not scoped to the calling project) and
Residuals 2–3 (a live session's bearer can be overwritten, and a bearer-omitting
`push` rides the resident one).
Basis for acceptance: all three are Low-severity, reachable only by a process already
running as the local user against a loopback-only surface, and only for a
`(server_url, project_id)` pair it must already have declared in the committed
`.inkentry/config.toml`; none is an exfiltration path, because the relay bearer is
returned by no handler (confirmed in this tree). The single fix that closes all three
— keying `RelayKey` on `(server_url, project_id, bearer)`, an architect decision
of 2026-08-14 — is deferred to 1.0.1 rather than landed in release
week, to avoid an unproven change to the relay session-identity and sync path days
before the tag.
