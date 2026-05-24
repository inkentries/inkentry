# spelunk-server HTTP API Contract

**Issue:** #261  
**Status:** Accepted — pending implementation

---

## Overview

This document specifies the HTTP API surface that `spelunk-cli` calls on
`spelunk-server`, the auth trait that allows us to replace API-key auth with
OAuth2 in future, and the changes required to existing endpoints.

The server already has a working memory API (`src/server/handlers.rs`). This
document covers:

1. The `AuthProvider` trait (replaces the inline `auth_middleware` function).
2. Changes to existing endpoints.
3. New endpoints needed for CLI integration.
4. The server's **data promise** to the CLI.

---

## Data promise (server → CLI)

The CLI is the only durable store for index data. The server's behaviour is
constrained as follows; cloud implementations MUST uphold the same contract.

| Resource | Server may receive | Server may store | Server may cache (in-memory, bounded TTL) |
|---|:---:|:---:|:---:|
| Chunk content (text) for embedding | yes | **no** | yes (eviction within session) |
| Embedding vectors generated for the CLI | n/a (server emits) | **no** | yes (request-scoped) |
| Search queries (text) | yes | **no** (logs may carry metadata only — never the query body in plaintext) | yes (LRU for rate limiting) |
| `context_chunks` sent with `/explore` and `/plan` | yes | **no** | request-scoped only |
| Memory entries (notes) | yes | **yes** (this IS the server's persistence role) | n/a |
| Project metadata (id, slug, stats) | yes | yes | n/a |
| Auth principals (api keys, user ids) | yes | hash/identifier only — never the raw bearer | yes |

**Rationale.** The server is an inference + memory peer, not a code-index
mirror. The CLI fingerprints every chunk locally (blake3) and writes vectors
to its own sqlite-vec store; the server has no business duplicating that.
Persisting embeddings server-side would also force the OSS deployment into a
data-residency conversation we do not want before cloud.

This promise is testable: server integration tests assert that the embedding
DB table is empty after `/v1/projects/{id}/index/embed` returns, and that
`/explore` writes nothing to memory.

---

## Auth architecture

### Current state (to be refactored)

Auth is currently implemented as a plain axum middleware function
(`auth_middleware`) that compares a bearer token against `AppState.api_key:
Option<String>`. This must be replaced with a trait so the auth strategy can be swapped
(e.g. OAuth2/JWT) without forking the repo.

### Target design

```rust
// crates/spelunk-server/src/auth.rs

/// Determines whether an incoming request is authorised and returns the
/// caller's identity. Implement this for each auth strategy.
#[async_trait::async_trait]
pub trait AuthProvider: Send + Sync + 'static {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<AuthContext, AuthError>;
}

/// Outcome of a successful authentication check.
pub struct AuthContext {
    pub principal: Principal,
}

/// Caller identity. Extensible for alternative auth strategies.
pub enum Principal {
    /// Default: bearer token matched the configured key.
    ApiKey(String),
    /// Future: authenticated user identity (e.g. OAuth2).
    User { id: String },
}

/// Authentication failed. Always maps to HTTP 401.
#[derive(Debug)]
pub struct AuthError(pub String);
```

`AppState` changes:

```rust
pub struct AppState {
    pub db: Arc<tokio::sync::Mutex<ServerDb>>,
    pub auth: Arc<dyn AuthProvider>,        // replaces api_key: Option<String>
    pub conflict_threshold: f32,
    pub embedder: Option<Arc<dyn EmbeddingBackend>>,
}
```

The auth middleware becomes:

```rust
async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    match state.auth.authenticate(request.headers()).await {
        Ok(ctx) => {
            request.extensions_mut().insert(ctx);
            next.run(request).await
        }
        Err(AuthError(msg)) => (StatusCode::UNAUTHORIZED, msg).into_response(),
    }
}
```

### OSS implementation: `ApiKeyAuth`

```rust
// crates/spelunk-server/src/auth.rs

pub struct ApiKeyAuth {
    /// None → accept all requests (no key configured; safe on a local loopback)
    key: Option<String>,
}

#[async_trait::async_trait]
impl AuthProvider for ApiKeyAuth {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<AuthContext, AuthError> {
        match &self.key {
            None => Ok(AuthContext { principal: Principal::ApiKey(String::new()) }),
            Some(expected) => {
                let provided = headers
                    .get("Authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "));
                match provided {
                    Some(t) if t == expected => {
                        Ok(AuthContext { principal: Principal::ApiKey(t.to_owned()) })
                    }
                    _ => Err(AuthError("Unauthorized".into())),
                }
            }
        }
    }
}
```

Any alternative auth strategy supplies its own `impl AuthProvider`. No changes
to handlers are needed.

### Server construction

```rust
// Binary entrypoint constructs the provider and passes it in:
let auth: Arc<dyn AuthProvider> = Arc::new(ApiKeyAuth::from_env());
let state = AppState { db, auth, conflict_threshold, embedder };
```

### Implementer wiring checklist

The complete trait and `ApiKeyAuth` impl above are the literal stub. Drop them
into a new file at:

```
crates/spelunk-server/src/auth.rs
```

…and add `pub mod auth;` to `crates/spelunk-server/src/lib.rs`. The existing
inline `auth_middleware` in `lib.rs` is replaced by the trait-driven version
shown above; `state.api_key: Option<String>` is removed in the same change.
No handler files (`handlers.rs`) require edits beyond switching consumers from
`api_key` reads to `Extension<AuthContext>` extraction where they care about
the principal (today, none do).

---

## Error response format

All error responses use a consistent JSON body. Update existing handlers to
emit this format (currently some return plain text):

```json
{ "error": { "code": "unauthorized", "message": "Bearer token required" } }
```

| HTTP status | `code` |
|---|---|
| 400 | `bad_request` |
| 401 | `unauthorized` |
| 404 | `not_found` |
| 409 | `conflict` (memory only — entry stored, conflicts returned) |
| 500 | `internal_error` |

---

## Changes to existing endpoints

### `GET /v1/health` — response format change

**Current:** returns `200 "ok"` (plain text, `&'static str`)

**New:** returns `200` JSON:

```json
{
  "status": "ok",
  "version": "0.8.0",
  "capabilities": [
    "memory",
    "index.embed",
    "search.semantic",
    "explore",
    "plan"
  ]
}
```

The `capabilities` array lists features this server instance supports. The CLI
uses this list to degrade gracefully against older server versions (see
capability-tiers.md). A server that supports only memory operations emits
`["memory"]`.

This is a **breaking change** for any client checking for the literal string
`"ok"`. The CLI's health probe must be updated to parse JSON.

### `POST /v1/projects/{project_id}/memory/search` — accept text query

**Current:** `SearchRequest { embedding: Vec<f32>, limit: usize }` — client
must supply a pre-computed vector.

**New:** `SearchRequest { query: String, limit: usize }` — server encodes the
query internally using its configured embedder.

```json
{ "query": "how does authentication work", "limit": 20 }
```

The old vector-based field is removed. Any client that was sending raw vectors
must migrate to text queries. This is safe because the only client is
`spelunk-cli` and the encoding it used was the same model the server runs.

If the server has no embedder configured, return:

```json
{ "error": { "code": "bad_request", "message": "This server has no embedder configured. Semantic memory search is unavailable." } }
```

---

## New endpoints

### `POST /v1/projects/{project_id}/index/embed`

Generate embeddings for code chunks. Called by the CLI during `spelunk index`
Phase 2. The server encodes each chunk and returns vectors. **The server does
not store the vectors** — the CLI is the only persistent store for index data.

**Request:**

```json
{
  "chunks": [
    {
      "chunk_id": "abc123",
      "content": "fn parse_config(path: &Path) -> Result<Config> { ... }"
    }
  ]
}
```

`chunk_id` is the CLI's local identifier (blake3 hash of file + offset). It is
opaque to the server and is echoed back so the CLI can match responses to its
local DB rows.

Maximum batch size: **256 chunks per request**. CLI must split larger batches.

**Response `200`:**

```json
{
  "chunks": [
    { "chunk_id": "abc123", "vector": [0.012, -0.034, ...] }
  ]
}
```

**Response `400`:** embedder not configured on this server.

**Response `413`:** batch exceeds 256 chunks.

If the server has no embedder, it returns 400:

```json
{ "error": { "code": "bad_request", "message": "index.embed requires an embedder. Configure SPELUNK_EMBEDDING_URL on the server." } }
```

### `POST /v1/projects/{project_id}/explore`

Run an LLM reasoning loop over caller-supplied context. The CLI retrieves
relevant chunks from its local index and sends them alongside the question.
**The server does not store context chunks.**

**Request:**

```json
{
  "question": "Why does the auth middleware bypass token check when api_key is None?",
  "context_chunks": [
    {
      "file": "src/server/mod.rs",
      "start_line": 140,
      "end_line": 165,
      "content": "async fn auth_middleware(...) { ... }"
    }
  ],
  "max_turns": 5
}
```

`context_chunks` is the pre-assembled retrieval context. The CLI is responsible
for fetching this from its local index before calling this endpoint.

**Response:** SSE stream, one JSON event per line:

```
data: {"kind":"thought","content":"The bypass exists to allow unauthenticated local use..."}

data: {"kind":"answer","content":"When no key is configured the server trusts all local callers..."}

data: {"kind":"done"}
```

Event `kind` values: `thought`, `answer`, `done`, `error`.

**Response `503`:** no LLM configured on this server.

### `POST /v1/projects/{project_id}/plan`

Generate a structured implementation plan via LLM. Same context assembly
pattern as explore.

**Request:**

```json
{
  "goal": "Add rate limiting to the memory push endpoint",
  "context_chunks": [ ... ],
  "style": "steps"
}
```

`style`: `"steps"` (numbered list, default) or `"checklist"` (markdown
checkbox list).

**Response `200`:**

```json
{
  "plan": {
    "title": "Add rate limiting to memory push",
    "steps": [
      "Add a `RateLimiter` struct to `spelunk-server/src/middleware/`...",
      ...
    ]
  }
}
```

**Response `503`:** no LLM configured on this server.

---

## Project identity

All per-project endpoints accept `project_id` as a URL path segment. The CLI
derives this from the `project_id` config field. Convention: `{owner}/{repo}`
(e.g. `acme/my-app`) — any slug is accepted, projects are auto-created on
first write.

---

## OpenAPI commitment

The server publishes its full contract at `GET /api-docs/openapi.json`
(already wired via `utoipa`). Every endpoint listed in this document MUST
appear there, with request/response schema components. The `utoipa::ApiDoc`
type in `crates/spelunk-server/src/lib.rs` is the source of truth — the
implementer extends it as endpoints land.

CLI integration tests pull `openapi.json` and assert presence + shape of:

- `paths./v1/health.get.responses.200.content.application/json.schema.properties.capabilities`
- `paths./v1/projects/{project_id}/index/embed.post`
- `paths./v1/projects/{project_id}/memory/search.post.requestBody` includes
  `query: string` (not `embedding: array`).

A snapshot of the generated `openapi.json` is committed under
`docs/openapi.json` and refreshed by the implementer on every change.
A CI check diffs the committed file against a freshly generated one.

---

## Endpoint summary

| Method | Path | Auth | Tier | Notes |
|---|---|---|---|---|
| `GET` | `/v1/health` | None | 0+1 | JSON response (breaking change from plain text) |
| `GET` | `/api-docs/openapi.json` | None | 0+1 | No change |
| `GET` | `/v1/projects` | Bearer | 1 | No change |
| `POST` | `/v1/projects/{id}/memory` | Bearer | 1 | No change |
| `GET` | `/v1/projects/{id}/memory` | Bearer | 1 | No change |
| `GET` | `/v1/projects/{id}/memory/{note_id}` | Bearer | 1 | No change |
| `DELETE` | `/v1/projects/{id}/memory/{note_id}` | Bearer | 1 | No change |
| `POST` | `/v1/projects/{id}/memory/{note_id}/archive` | Bearer | 1 | No change |
| `POST` | `/v1/projects/{id}/memory/{note_id}/supersede` | Bearer | 1 | No change |
| `POST` | `/v1/projects/{id}/memory/search` | Bearer | 1 | **Breaking:** text query replaces vector |
| `GET` | `/v1/projects/{id}/memory/since` | Bearer | 1 | No change |
| `GET` | `/v1/projects/{id}/memory/stream` | Bearer | 1 | No change |
| `GET` | `/v1/projects/{id}/memory/harvested-shas` | Bearer | 1 | No change |
| `GET` | `/v1/projects/{id}/stats` | Bearer | 1 | No change |
| `POST` | `/v1/projects/{id}/index/embed` | Bearer | 1 | **New** |
| `POST` | `/v1/projects/{id}/explore` | Bearer | 1 | **New** (SSE) |
| `POST` | `/v1/projects/{id}/plan` | Bearer | 1 | **New** |

---

## Definition of done

- [ ] `AuthProvider` trait defined in `spelunk-server/src/auth.rs`
- [ ] `ApiKeyAuth` struct implements `AuthProvider` (behaviour unchanged from
  current `auth_middleware`)
- [ ] `AppState.api_key` replaced with `AppState.auth: Arc<dyn AuthProvider>`
- [ ] `GET /v1/health` returns JSON with `capabilities` array
- [ ] `POST /v1/projects/{id}/memory/search` accepts `{"query": String}`
- [ ] `POST /v1/projects/{id}/index/embed` implemented
- [ ] `POST /v1/projects/{id}/explore` implemented (SSE)
- [ ] `POST /v1/projects/{id}/plan` implemented
- [ ] Error responses use `{"error": {"code": "...", "message": "..."}}` format
- [ ] OpenAPI spec updated for all new/changed endpoints
- [ ] All existing `cargo test` suites pass; `cargo fmt` + `cargo clippy` clean
