# inkentry Threat Model

**Method:** Lightweight threat modeling (STRIDE-informed)  
**Last reviewed:** July 2026 (transport model updated to native in-process HTTPS, ADR-066; egress model corrected to the server-owned embedding path, ADR-002; `api_base_url` retired; the embedding model and its compute path pinned product-wide, `--embedding-url` / `INKENTRY_EMBEDDING_URL` removed: embedding can no longer egress to a third party)  
**Reviewed by:** Architect  
**Next review:** v1.0 release or after any new network-facing feature

---

## System Overview

inkentry has two distinct operational modes with different attack surfaces:

### Mode A — Local CLI (default)
1. Walks source trees, parses files with tree-sitter, stores chunks in SQLite
2. Embeds chunks by sending chunk text to a `inkentry-server` over HTTP (ADR-002; the CLI never embeds in-process). The default auto-discovered loopback server embeds natively in-process (bundled F2LLM), so chunk text does **not** leave the machine.
3. Runs KNN search over stored embeddings via sqlite-vec
4. Optionally sends context + a user question to the same `inkentry-server`'s LLM endpoint
5. Maintains a `memory.db` of structured notes with semantic search. **`memory.db` is the single authoritative memory store at the CLI tier** (ADR-004). All `inkentry memory` operations (add, list, search, timeline, harvest) read from and write to `memory.db`.
6. **When `store_in_git_notes = true` (the default):** each `inkentry memory add` also appends the note as a JSON line to `refs/notes/inkentry` on HEAD (PR #339). Git notes in this namespace travel with the repository on `git push` and are available to anyone who clones the repo — see [git-notes memory](#git-notes-memory-prref-noteinkentry) below.

**Auto-discovered loopback inkentry-server (v0.8.0+):** inkentry auto-starts a local `inkentry-server` daemon (bound to `127.0.0.1`) to provide a native embedder and LLM backend. This server is **inference-only**: it receives query text or chunk text for embedding, and completion prompts for LLM calls. It does **not** receive note text for storage and is **not** a memory backend. Only an explicit `server_url` in config (pointing at a team or cloud server) moves the memory store of record away from `memory.db`.

### Mode B — inkentry-server
An axum HTTP API (`src/server/`) that exposes memory CRUD and semantic search over the network:
- Binds to a configurable interface/port; intended for shared team use. Loopback binds serve plaintext HTTP; a non-loopback bind serves HTTPS in-process (ADR-066) via `--tls-cert`/`--tls-key`
- Bearer token authentication (`--key` / `INKENTRY_SERVER_KEY`). Unauthenticated is permitted **only on a loopback bind**; a non-loopback bind is refused unless **both** TLS and a key are set (see "Key difference" below)
- Accepts pre-computed embedding vectors from clients (clients embed locally, server stores and searches)
- Serves multiple projects via project_id routing
- Exposes: `POST /v1/projects/{id}/memory`, `POST /v1/projects/{id}/memory/search`, DELETE, archive, supersede

### Backend configurability
All inference goes through `inkentry-server` (ADR-002); the CLI has no direct
embedding/LLM endpoint of its own. Egress off the local machine happens in two
cases, both of which change the data-egress threat profile:

- **Explicit team `server_url`** in config points the CLI at a remote
  `inkentry-server`. Chunk text and query text then cross the network to that
  server, which embeds them natively in-process (embedding has no external
  relocation option; see below).
- **Server-side external LLM shim:** a `inkentry-server` operator may set
  `--llm-url` (`INKENTRY_LLM_URL`) so the server forwards LLM calls to a
  third-party OpenAI-compatible service (OpenAI, Anthropic, Cohere, etc.).
  This is configured on the server, not by the client, and applies to LLM
  features only: embedding has no equivalent flag and always runs on the
  server that receives the chunk/query text.

The default auto-discovered loopback server embeds natively in-process with no
external egress.

---

## Assets

| Asset | Confidentiality | Integrity | Availability |
|-------|:-:|:-:|:-:|
| Source code chunks in index | Medium | High | Medium |
| Credentials accidentally present in source | High | — | — |
| Memory notes (decisions, handoffs) | Medium | High | Medium |
| **git-notes memory (`refs/notes/inkentry`)** | **Medium–High** | Medium | Low |
| Embedding vectors | Low | Medium | Low |
| inkentry config (`~/.config/inkentry/config.toml`) | Medium | High | Medium |
| Server-side memory DB (all projects) | High | High | High |
| Bearer token / API key (server mode) | High | — | — |

**Note on git-notes confidentiality:** Notes may contain architectural decisions, credentials accidentally typed into `--body`, handoff text referencing internal systems, or other context a developer would not ordinarily commit to the repo. If the repo is pushed to a shared or public remote the notes are readable by anyone with clone access.

---

## Trust Boundaries and Data Flows

### Mode A — Local CLI

```
User filesystem
  │
  ├─ inkentry index ─► [secret scanner] ─► SQLite index.db (chunks + vectors)
  │                                              │
  │                                              └─► embed chunk text via HTTP ─► inkentry-server
  │                                                   (loopback: native, on-machine;
  │                                                    team server_url: leaves the machine)
  ├─ inkentry search
  │     ├─► embed query text via HTTP ─► inkentry-server
  │     │    (chunk + query text leave the machine only if server_url is a remote
  │     │     team server; that server always embeds natively, never proxies)
  │     ├─► KNN search ─► index.db  (always local sqlite-vec)
  │     └─► LLM prompt ─► inkentry-server
  │           └─ context: code chunks + spec files + memory notes
  │
  ├─ inkentry memory add ─► memory.db (SQLite, local)  ← single canonical store (ADR-004)
  │                     └─► [git notes append] ─► refs/notes/inkentry on HEAD
  │                                                       │
  │                                                       └─► git push ─► remote (any clone)
  │                                                            ┌──────────────────────────────────────┐
  │                                                            │ TRUST BOUNDARY: local repo → remote  │
  │                                                            │ Notes travel with the repo;           │
  │                                                            │ no secret scan on this path (*)       │
  │                                                            └──────────────────────────────────────┘
  │
  └─ inkentry search (memory corpus)
        ├─► embed query via HTTP ─► loopback inkentry-server (inference-only)
        │    (query text only; note content stays in memory.db — NOT sent to server)
        └─► KNN search ─► memory.db (local sqlite-vec)
```
(*) `inkentry harvest` (harvest_claude.rs) does run `contains_secret` on
harvested text before storing. Direct `inkentry memory add` does **not** — the
note body comes from the user's own command line or `$EDITOR` and is written to
git notes verbatim.

**Memory data-flow rule (ADR-004):** Note text for storage is never sent to the
loopback inkentry-server. For `memory search`, only the query string crosses the
loopback trust boundary (to obtain a query embedding); the KNN search and all
note reads/writes operate on the local `memory.db`. If a team `server_url` is
explicitly configured, memory moves to that server instead — see Mode B.

### Mode B — inkentry-server

```
Client (inkentry CLI / any HTTP client)
  │
  ├─► POST /v1/projects/{id}/memory        — store note + pre-computed embedding
  ├─► POST /v1/projects/{id}/memory/search — KNN search by embedding vector
  ├─► GET  /v1/projects/{id}/memory        — list notes
  └─► DELETE / archive / supersede         — mutate note state
         │
         ▼
  inkentry-server (axum, bound to configured port)
    ├─ auth_middleware (bearer token, optional)
    └─ ServerDb (SQLite, server-local)
```

**Key difference from Mode A:** In server mode, memory content is accessible to anyone
who can reach the server's port. The bind guard (`check_bind_safety`) encodes the
local/remote boundary directly (ADR-066 §4):

| Bind | TLS configured | Key set | Result |
|---|---|---|---|
| loopback | any | any | allow (local plaintext HTTP, no key needed) |
| non-loopback | no | any | refuse (no plaintext off-host, keyed or not) |
| non-loopback | yes | no | refuse (remote requires an API key) |
| non-loopback | yes | yes | allow (remote HTTPS, key required) |

So a non-loopback bind is allowed **only** when the server terminates HTTPS
itself (`--tls-cert`/`--tls-key`) **and** an API key is set: this keeps the bearer
key off the wire in cleartext and prevents an open, unauthenticated server. A
keyless or plaintext server can therefore only bind loopback (`127.0.0.1`), where
it is reachable by local processes but not by other machines. (A blank or
whitespace key, e.g. docker-compose's `${INKENTRY_SERVER_KEY:-}` default, is
treated as no key.)

### Tenancy boundary: single trust domain (ADR-056)

A `inkentry-server` instance is a **single trust domain**, and its shared key is
the tenancy boundary. This is a deliberate design decision recorded in
[ADR-056](../adr/056-oss-server-tenancy-model.md), not an unimplemented control:

- Holding the server's key grants full participation in **every** project on
  that instance: list, read, search, write, supersede, archive, and delete.
  `GET /v1/projects` enumerating all project slugs is intended behaviour.
- The `project_id` slug in the request path is an **addressing convenience, not
  a security boundary**. The server implements no per-project or per-principal
  authorization, because the OSS SQLite server has no identity, org, or role
  model to hang one on.
- Isolation between teams or projects that must not see each other's memory is
  achieved by running **separate server instances**, each with its own key and
  its own database.
- Consequently, cross-project read/write on a single instance is documented,
  intended behaviour under this model, not a vulnerability. A future ADR that
  introduces a scoped-key and ACL model would supersede this decision.

**Transport (ADR-056 addendum, updated by ADR-066):** the server serves plaintext
HTTP only on a loopback bind. A shared, non-loopback deployment terminates TLS
**in-process** (`--tls-cert`/`--tls-key`, ADR-066) so the shared key never crosses
the network in cleartext, with nothing in front of the server. `/v1/health` is
unauthenticated (no bearer required or sent).

---

## Threat Analysis (STRIDE)

### S — Spoofing

| Threat | Mode | Likelihood | Impact | Mitigation |
|--------|------|-----------|--------|-----------|
| Client impersonates a legitimate inkentry user to the server | B | Medium | High | Bearer token auth — but **optional**; server runs unauthenticated by default. Operators must explicitly pass `--key` / `INKENTRY_SERVER_KEY`. |
| Attacker spoofs the embedding/LLM backend to return adversarial responses | A | Low | Medium | The loopback server is on-machine, so this only applies when a remote team `server_url` (or a server's external `--llm-url`) is used over plaintext HTTP. `validate_transport_url` rejects a non-loopback `http://` `server_url` (loopback-only plaintext; https required otherwise), so a remote backend must be HTTPS. |

### T — Tampering

| Threat | Mode | Likelihood | Impact | Mitigation |
|--------|------|-----------|--------|-----------|
| Malicious chunk content injects SQL | A | Low | High | All DB writes use rusqlite parameterised queries — no string formatting into SQL |
| `memory.db` edited directly to corrupt supersession state | A | Low | Medium | Atomic transactions in `insert_with_supersession()` and `supersede()` (issue #136) |
| Unauthenticated HTTP client corrupts server memory DB | B | Medium | High | Bearer token auth — but optional. Unauthenticated by default. |
| Embedding server returns malformed vectors | A/B | Low | Low | Dimension validation on KNN input; errors surface as HTTP 400 (server) or exit 2 (CLI) |
| **git notes rewritten by another tool or git command, corrupting stored memory** | A | Low | Medium | `inkentry memory add` uses `git notes add -f` (force-replace) per-commit. A concurrent `git notes add` or `git notes prune` from another process could silently drop entries. The git-notes backend is documented as unsuitable for concurrent multi-agent use (#185); the SQLite backend is the recommended default for such workflows. |

### R — Repudiation

| Threat | Mode | Likelihood | Impact | Mitigation |
|--------|------|-----------|--------|-----------|
| No record of who created/deleted a memory note on the server | B | Medium | Medium | Server has no per-request audit log. `source_ref` field can record commit SHA but is not required. Under the single-trust-domain model (ADR-056) every keyholder is a full administrator, so per-principal attribution is not an isolation control; `created_by` / request logging remains a possible future operational aid for shared deployments. |

### I — Information Disclosure

| Threat | Mode | Likelihood | Impact | Mitigation |
|--------|------|-----------|--------|-----------|
| Credentials in source code indexed into vector DB | A | Medium | High | `secrets.rs` scanner drops matching chunks before storage; `.env*`/`*.pem`/`*.key` files excluded |
| **Source code sent off-machine for embedding** | A | Medium | **High** | The default loopback server embeds natively on-machine, so nothing leaves. Egress requires an explicit remote team `server_url` (chunk text crosses to that server, which always embeds natively in-process; there is no operator flag to forward embedding to a third party). This is an explicit operator/user choice; users must be informed via docs. **Enforced** for the local-tier default: `crates/inkentry-cli/tests/egress_containment.rs` traps every outbound connection across `init`/`index`/`search` and fails loudly, naming the destination, on any escape past loopback. |
| **Memory notes / code context sent off-machine for LLM** | A | Low | **High** | `inkentry harvest` sends memory content + code context to `inkentry-server`. On the default loopback server the LLM runs on-machine; egress requires a remote team `server_url`, or an `llm_url` (config key, `INKENTRY_LLM_URL`, or `--llm-url`) pointing off-machine. Either is an explicit user choice, and an `llm_url` is never inherited from a checked-in project config: it is read from the personal config only, so cloning a repo cannot redirect a developer's LLM traffic. |
| Server memory accessible without auth | B | Medium | High | No `--key` / `INKENTRY_SERVER_KEY` by default; any process that can reach the port reads all notes |
| Server bound to 0.0.0.0 exposes data on LAN/internet | B | Medium | High | **Enforced:** a non-loopback bind requires **both** TLS and a key: `inkentry-server` refuses to start on `0.0.0.0`/LAN/public addresses unless `--tls-cert`/`--tls-key` and `--key` / `INKENTRY_SERVER_KEY` are set (ADR-066 §4); plaintext off-host is refused with no override; loopback (`127.0.0.1`) is the default (PR #490) |
| Indexed content contains credentials missed by scanner | A | Medium | Medium | Pattern gaps tracked in #138 |
| CLI bearer credential (`server_key`) readable as plaintext at rest (e.g. user syncs `~/.config` into a dotfiles repo or backup) | A | Medium | High | The `server_key` is stored in the OS keychain (macOS Keychain / Linux Secret Service / Windows Credential Manager), not in `config.toml`; a legacy plaintext key is migrated out and stripped on next run. Headless fallback is an owner-only (`0600`) `secrets.toml`; `INKENTRY_SERVER_KEY` is the CI escape hatch. The credential is never logged. |
| LLM endpoint credential (`llm_url`) exposed in the process table, at rest, or in transit | A | Medium | High | Stored in the OS secret store via `inkentry auth set-key --llm`, never in `config.toml`; read from stdin/prompt and refused as an argument. The CLI resolves it only on the daemon-spawn path and passes it to the child in its environment: no input emits `--llm-key`/`--llm-key-file` into the spawned daemon's argv, and the endpoint URL/model travel as arguments precisely because they are not secret. `INKENTRY_LLM_KEY` is the CI/non-interactive escape hatch. Never logged at any level, and not echoed by the refusal below. When a credential resolves against a plaintext `http://` non-loopback endpoint, `inkentry-server` refuses to start rather than sending it in the clear; the check is scoped to a credential being present, so keyless LAN endpoints are unaffected. |
| Detached `inkentry-server` daemon reads the OS keychain, raising an authorization prompt no user can answer (or, worse, being granted standing access) | A | Medium | Medium | **Structural:** the server crate reaches for no secret store at all. The CLI resolves the credential in the user's own session and hands it over out of band. Enforced by `the_server_crate_never_reaches_for_a_secret_store`, a source-level scan of `crates/inkentry-server/src/`, so a future reach fails CI rather than shipping. |
| `inkentry memory add`/edit interactive `$EDITOR` draft written to a predictable temp path, enabling symlink/TOCTOU clobber and a world-readable info-leak window | A | Low | Medium | **Fixed:** the draft is created via `tempfile::Builder` (unpredictable name, `O_EXCL`, mode `0600` on unix) instead of a PID-derived path in `std::env::temp_dir()`. The `NamedTempFile` handle is kept open across the `$EDITOR`/`$VISUAL` spawn and the body is read back by seeking the retained handle (not by re-opening the path), so a symlink swapped in at the draft's path during the edit window is not followed. |
| **Memory note body contains a credential written to git notes and pushed to a shared/public remote** | A | **Medium** | **High** | **No mitigation on the direct `memory add` path.** The `store_in_git_notes` flag is `true` by default. `contains_secret` is not called in `add.rs` before `append_to_git_notes`. Users must set `store_in_git_notes = false` in config to opt out, or avoid including secrets in note bodies. See [git-notes memory](#git-notes-memory-prref-noteinkentry) section. Track: issue to add secret-scan gate on write-through path. |
| **Sensitive architectural context (decisions, handoffs) in git notes exposed on clone to any repo reader** | A | **Medium** | **Medium** | Notes attached to `refs/notes/inkentry` are fetched by `git fetch` when the refspec is included; anyone with clone access reads the full history of notes. **Documentation control only** — users must understand that `store_in_git_notes = true` (default) means notes are as public as the repo. |

### E — Elevation of Privilege

| Threat | Mode | Likelihood | Impact | Mitigation |
|--------|------|-----------|--------|-----------|
| Path traversal via project_id or note body to read arbitrary server files | B | Low | High | The `project_id` is a slug used only as a database key (capped in length at the handler), never as a filesystem path; the note body is stored as-is but never executed. No file reads derive from user-supplied request fields. |
| Keyholder reads or deletes another project's memory on a shared instance | B | n/a | n/a | **Intended behaviour, not a defect (ADR-056).** A server instance is a single trust domain; the shared key grants full access to every project. Teams that must be isolated run separate instances. This is not an elevation of privilege because there is no lower privilege level to elevate from: one key is one trust domain. |
| Git argument injection via `inkentry harvest --branch`/`--git-range` (e.g. `--branch=--output=<path>`) forwarded to `git log` with no `--` separator, letting an option-shaped value be parsed as a git flag instead of a ref (arbitrary local file clobber) | A | Low | Medium | Fixed: `reject_option_like_ref()` rejects any ref, or either endpoint of an `A..B` range, starting with `-` before the subprocess spawns; both `git log` invocations also append a trailing `--` separator as defense-in-depth. Same review applied to the git-notes write path (`git_notes/mod.rs`): all `<object>` args to `notes show`/`add` are `--`-guarded, and note bodies are written via stdin (`-F -`) instead of `-m <arg>`, so a body can't be argv-parsed as an option or exposed on `ps`. |

### D — Denial of Service

| Threat | Mode | Likelihood | Impact | Mitigation |
|--------|------|-----------|--------|-----------|
| Client floods server with large embedding vectors | B | Low | Medium | Fixed: `add_note` rejects a request whose embedding vector length doesn't equal the server's configured dim (400), and `RequestBodyLimitLayer` caps every request body at 2 MiB regardless of route. |
| Slow/hung client or backend holds a request open, exhausting server capacity | B | Low | Medium | Fixed: `TimeoutLayer` (30s) on `router()` bounds every route except `/memory/stream` (exempted — it's a deliberate long-lived SSE connection), and a global `ConcurrencyLimitLayer` (256 concurrent requests) backpressures the router as a whole. `/llm/complete` hands generation to a detached `tokio::spawn`, so the outer `TimeoutLayer` alone doesn't bound a hung LLM backend on that route (the wrapped `Future` resolves once the streaming `Response` is constructed, before generation starts) — closed by wrapping the spawned `generate()` call itself in `tokio::time::timeout(REQUEST_TIMEOUT, ...)` (`llm_generate_with_timeout`). **Known limitation:** `ConcurrencyLimitLayer` has the same structural blind spot on `/llm/complete` — its permit releases at `Response` construction, not stream completion, so it doesn't actually bound the number of concurrent in-flight *streaming sessions* on that route. Bounding that needs a dedicated semaphore held for the stream's lifetime; not yet implemented. `/index/embed` is now **also** exempted from the general 30s budget: it gets its own `EMBED_REQUEST_TIMEOUT` (1800s, matching the CLI's own calibrated ceiling), because a legitimate embed batch on slow/CPU-only hardware genuinely needs minutes, not seconds. This does not reopen the DoS gap — `/index/embed` stays behind the same `auth_middleware` + `ConcurrencyLimitLayer` + `RequestBodyLimitLayer(2 MiB)` + the handler's own `MAX_EMBED_BATCH` (256 chunks) cap, so the worst case is bounded (`GLOBAL_CONCURRENCY_LIMIT` requests, each ≤256 chunks / 2 MiB, each held for ≤1800s) rather than unbounded. `/v1/health`'s `limits` object advertises `embed_request_timeout_secs`/`max_batch_chunks`/`embedder_token_cap` so a client can size its batches to the server it's actually talking to instead of assuming. |
| `/explore` unmetered LLM token-burn proxy (surface removed) | B | — | — | **Surface removed** (ADR-079): the `/explore` route no longer exists. The generic `/llm/complete` route remains and keys its rate-limit bucket on **principal + client address** (`"<principal>|<ip>"`) rather than principal alone, so a shared team key no longer collapses every distinct caller onto one global bucket. |
| Rate limit bypassed by setting a request header | B | High | High | **Fixed.** The address half of the bucket key used to come from the leading `X-Forwarded-For` entry whenever one was present, justified by an assumed reverse proxy. ADR-066 rejected that shape in favour of in-process TLS, so no proxy exists to trust, and the header is client-supplied: varying it per request minted a fresh budget and removed the ADR-002 rate limit entirely (measured: budget of 2, then 200 requests served). The address now comes from the TCP peer (`ConnectInfo`). `X-Forwarded-For` is honoured only when the operator names the peer via `--trusted-proxy`/`INKENTRY_TRUSTED_PROXIES`, and only when the entry parses as an IP address. |
| Rate-limiter map grows without bound | B | Medium | Medium | **Fixed.** Bucket keys were arbitrary attacker-chosen strings (the unvalidated forwarded value), each allocating a `HashMap` entry retained for the process lifetime (measured: 500 buckets from one caller with 200-byte keys). Forwarded values must now parse as an `IpAddr`, so key length and cardinality are both bounded; expired windows are swept (at most once per window, and under insert pressure) and the map is hard-capped at `DEFAULT_MAX_BUCKETS`, evicting the window nearest expiry rather than refusing a new caller. |
| Oversized `title`/`body` on a memory write | B | Low | Low | Fixed: `add_note` enforces `MAX_TITLE_LEN` (500 chars) and `MAX_BODY_LEN` (50,000 chars) at the handler, returning 400 on violation (see [§4 Input validation](V1-SERVER-AUDIT.md#4-input-validation)). |
| Single global `tokio::sync::Mutex<ServerDb>` serializes all DB access; one slow query blocks every request, including the `/memory/stream` SSE poll loop | B | Medium | Medium | **Not mitigated — accepted follow-up.** The concurrency cap above partially bounds concurrent load in the meantime, but the mutex itself is unchanged. A read-pool refactor (replacing the single mutex with a connection pool) is a larger structural change and is not yet implemented. |

---

## Prompt Injection

| Threat | Mode | Likelihood | Impact | Mitigation |
|--------|------|-----------|--------|-----------|
| Indexed source file contains adversarial LLM instructions | A | Low | Medium | XML delimiter isolation in `ask.rs`; angle-bracket escaping of retrieved context (issue #137) |
| Indexed source file steers an in-process LLM `read_file` tool into reading an arbitrary path (e.g. `/Users/me/.ssh/id_rsa`, `../../etc/passwd`), exfiltrating file contents | A | — | — | **Surface removed** (ADR-079): the `explore` command that ran an in-process file-reading tool loop no longer exists, so there is no server-side `read_file` boundary to enforce. Multi-hop retrieval is now a caller-run skill; the equivalent norm — read only files inside the project — is documented as the caller's responsibility in `SKILL.md`. |
| User query contains injection payload | A | Low | Low | Pre-flight check against known patterns (`ask.rs` lines 155–174) |
| Memory note stored via team server contains injection payload, later surfaced to the caller's agent (e.g. via `inkentry search` / `context`) | B | Low | Medium | Applies only when an explicit team `server_url` is configured (Mode B). In Mode A, notes are stored in local `memory.db`, not via the loopback server, so this attack requires access to the user's filesystem. Retrieved notes are untrusted content; the consuming agent must apply its own delimiter isolation / escaping when placing them into an LLM prompt. |

**Residual risk:** Pre-flight only blocks known string patterns. Novel injection payloads in indexed content or memory notes could influence the LLM response.

---

## Generic inference endpoint — `POST /v1/projects/{id}/llm/complete` (Mode B, ADR-002)

ADR-002 adds a generic LLM completion primitive to `inkentry-server` so the CLI
can route `inkentry harvest` (and future inference-needing commands)
through one stable route instead of a bespoke endpoint per command. This
introduces a **new trust boundary**: a network-facing, free-form inference
endpoint that runs arbitrary caller-supplied prompts against the server's
configured (possibly BYOK, possibly metered) LLM.

This is a deliberately broader surface than a scoped `/harvest` endpoint would
be. The trade-off is accepted **only** with the controls below; they are
binding requirements, not recommendations.

| Threat | STRIDE | Likelihood | Impact | Mitigation (binding) |
|--------|--------|-----------|--------|----------------------|
| Authenticated caller runs arbitrary prompts to burn the operator's LLM budget | D / EoP | Medium | Medium | Tier-1 + Bearer auth required; **rate limit keyed on principal + client IP** (a shared team key no longer shares one global bucket) + token budget; client `max_tokens` **clamped** to a server-side ceiling (never trusted upward) |
| Caller exfiltrates or abuses a BYOK upstream key | I | Low | High | BYOK key **never leaves the server** — client sends prompts, server holds the upstream key; stored as HMAC-SHA256 hash, resolved via Secret Manager in cloud, never logged (decisions #25/#26) |
| Prompt injection via caller-supplied `messages` | T | Medium | Medium | `llm/complete` is a **raw** primitive: the server adds **no** system prompt and makes **no** trust assumptions. Delimiter isolation / angle-bracket escaping of untrusted context is the **caller's** responsibility (issue #137). The server must NOT wrap or re-prompt content. |
| Completion content or prompts persisted/leaked server-side | I | Low | Medium | No persistence: messages are request-scoped, never written to the memory DB, never logged in plaintext (same data-promise as `/index/embed`) |
| Unconfigured server invoked | — | Low | Low | `503 llm_unavailable` when no LLM backend configured; endpoint absent from `/v1/health` `capabilities` so the CLI gates it |

**Why generic over per-command (security framing):** a bespoke `/harvest` would
narrow the input shape but would force harvest's ~2300 LoC of prompt
orchestration across the trust boundary into the server, expanding the
server's attack surface and duplicating CLI logic. Keeping orchestration in the
CLI and exposing only a raw, auth-gated, rate-limited, non-persisting primitive
is the smaller *server-side* trust surface, at the cost of a broader *input*
surface — which the controls above contain. See ADR-002 for the full rationale.

**Cost attribution** is per-principal via `AuthContext` (#261 auth trait) — the
same granularity a bespoke endpoint would provide. No attribution granularity is
lost by going generic.

---

## git-notes memory (`refs/notes/inkentry`)

PR #339 introduced a write-through that persists every `inkentry memory add` entry
as a JSON line appended to `refs/notes/inkentry` on HEAD when `store_in_git_notes = true`
(the default). This section models the associated data flows and trust boundaries.

### What is stored

A commit's note is JSON Lines: one `NoteRecord` per line (canonical inkentry
format), possibly interleaved with foreign content (prose, other tools' lines).
Each record contains: `id`, `kind`, `title`, `body`, `tags`, `linked_files`,
`created_at`, `status`, `source_ref`, an optional `remote_id` (the canonical
cross-machine id, present only once an entry is synced to a remote server), and
schema metadata. The `body` field is the raw user-supplied text from `--body`
or `$EDITOR`. Reads skip foreign lines without erroring; writes preserve every
foreign line and every untargeted record verbatim.

### How notes propagate

```
inkentry memory add
  └─► append_to_git_notes() in storage/git_notes.rs
        ├─► git notes --ref=inkentry show HEAD   (read existing blob)
        ├─► append the new record as one JSON line, keeping all prior lines
        └─► git notes --ref=inkentry add -f HEAD (write back)

git push [with refs/notes/inkentry in refspec or push.followTags / notes config]
  └─► remote repository — readable by anyone with clone access
```

Git does not push notes by default unless the user explicitly configures
`remote.<name>.push = refs/notes/*` or passes `refs/notes/inkentry` on the
command line. However, inkentry's documentation uses `git push --tags` and
`git push` patterns that do not push notes unless configured — but many CI
systems and IDE integrations push all refs. Users should be aware of their
push configuration.

### Trust boundary

| Boundary | Direction | What crosses it |
|----------|-----------|-----------------|
| Local git repo → git remote | On `git push` (when notes refspec is included) | All `NoteRecord` JSON attached to pushed commits |
| git remote → any clone | On `git clone` / `git fetch` with notes refspec | Same NoteRecord JSON |

### Secret-scanning status on this path

| Code path | Scanner called? | Notes |
|-----------|:-:|-------|
| `inkentry index` (chunk storage) | Yes — `contains_secret()` in `parse_phase.rs` | Credentials dropped before DB write |
| `inkentry harvest` (harvest_claude.rs) | Yes — `contains_secret()` before storing | Harvested bodies screened |
| `inkentry memory add` → git-notes write-through | **No** | Body is user-supplied text written verbatim to `refs/notes/inkentry`. No call to `contains_secret()` exists in `add.rs` before `append_to_git_notes()`. |

**Risk:** A user who types `inkentry memory add --title "DB creds" --body "password=s3cr3t"` will
have that credential stored verbatim in `refs/notes/inkentry` and, if the repo is
pushed with notes, the credential is exfiltrated.

### Controls and recommendations

| Control | Status |
|---------|--------|
| Secret scanning on `memory add` write-through path | **Gap — not implemented.** Binding requirement #8 below tracks this. |
| `store_in_git_notes = false` opt-out | Available in `~/.config/inkentry/config.toml`; not the default. |
| Documentation warning that notes travel with the repo | Added in `docs/memory.md` and `SKILL.md` (PR #276). |
| `git push` does not push notes by default | True — but not a reliable control; depends on user's git config. |

---

## Third-Party Backend Risk (all modes)

The default backend is on-machine (loopback `inkentry-server`, native F2LLM
embedder), so by default no code or memory content leaves the machine. This
section covers the two paths that reach a third party. Embedding has no
third-party path at all: it is always computed natively, in-process, by
whichever `inkentry-server` receives the text; the control is the choice of
`server_url`, not a server-side embedding flag.

**When a remote team `server_url` is set (chunk/query text and memory context
cross to that server), or a `inkentry-server` operator has set an external
`--llm-url` shim (e.g. `https://api.openai.com`) for LLM features only:**

| Data sent | Trigger | Risk |
|-----------|---------|------|
| Source code chunk content (post-secret-scan) | `inkentry index` against a remote team `server_url` | Code exfiltration to that server |
| User query text | `inkentry search` against a remote team `server_url` | Query logging by that server |
| Code context + memory notes | `inkentry harvest`, via a remote team `server_url` and/or a server-side `--llm-url` LLM shim | Combined context exfiltration |
| Memory note bodies | `inkentry harvest`, via a remote team `server_url` and/or a server-side `--llm-url` LLM shim | Decision/requirement exfiltration |

**Mitigations (documentation, not code):**
- Document the data-egress implications prominently in `docs/getting-started.md` and the `config.toml` comments
- The default (no `server_url`, auto loopback server, native embedder) keeps all code and memory on-machine; reaching a third party is an explicit operator choice
- Secret scanning reduces but does not eliminate the risk — it only drops chunks matching known credential patterns

**Recommended future control:** Add a `data_classification = "local-only"` config flag that refuses to configure a non-loopback `server_url`, with an explicit opt-in override.

---

## Out-of-Scope Threats

- Remote code execution via the embedding/LLM server (that server is user/operator-controlled)
- Compromised Rust crate supply chain (covered by `cargo audit`/`cargo deny`)

---

## Security Requirement Derivations

From this threat model, the following requirements are binding:

1. **No SQL string formatting.** All DB operations use rusqlite parameterised queries.
2. **Secret scanner must run before every DB write of chunk content.** Enforced in `parse_phase.rs`.
3. **LLM context must use XML delimiters** with angle-bracket escaping of all retrieved content (issue #137).
4. **Atomic transactions for memory state transitions** — `supersede()` and `insert_with_supersession()` (issue #136).
5. **CI must gate on `cargo audit` and `cargo deny`.**
6. **inkentry-server documentation must warn** that the server is unauthenticated by default and should only be exposed beyond localhost when `--key` / `INKENTRY_SERVER_KEY` is set.
7. **Config documentation must warn** that setting a remote team `server_url` (or running a `inkentry-server` with an external `--llm-url` shim) transmits source code and memory content off the machine.
8. **Secret scanner must run on the git-notes write-through path.** `add.rs` must call `contains_secret(body)` (and optionally `contains_secret(title)`) before calling `append_to_git_notes()`. If a match is found, the git-notes write must be skipped (with a `tracing::warn!`) and the primary SQLite write must still succeed. This closes the gap identified in the [git-notes memory](#git-notes-memory-prref-noteinkentry) section above. **This is a binding requirement for any release with `store_in_git_notes = true` as the default.**
