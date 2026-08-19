# CLI Capability Tiers

**Issue:** #259  
**Status:** Implemented (v0.8.0)

---

## Overview

The inkentry CLI operates in one of two capability tiers determined at runtime
by whether a `inkentry-server` is reachable. No compile-time feature flags are
used; the binary is the same in both tiers.

| | Tier 0 — Offline | Tier 1 — Server-connected |
|---|---|---|
| **Condition** | No `server_url` configured, or server unreachable | `server_url` set and health probe succeeds |
| **Search** | BM25 text over the local index | + semantic KNN (server encodes query, CLI does local KNN) |
| **Index** | Parse + AST chunk + graph (no embeddings) | + embedding phase: server generates vectors, CLI stores in local DB |
| **Memory add/list/show/archive** | sqlite (local) | Same |
| **Memory transfer** (`plumbing push`/`pull`, `sync`) | Not available | Push/pull entries to/from the server DB |
| **Memory corpus of `search`** | Text matching over the local `memory.db` | Server encodes query, CLI does KNN over the local `memory.db` |
| **Memory harvest** | Not available | LLM extraction via server |

**The CLI never calls embedding or LLM APIs directly, regardless of
configuration.** All inference routes through `inkentry-server`.

> **Reserved: Plan.** `/plan` is reserved as a server-owned route per ADR-002,
> but nothing ships today: there is no `inkentry plan` subcommand and no `/plan`
> server route. The CLI parses a `plan` capability from the server health
> response but deliberately keeps it out of all output, so it never surfaces.

**Tier 0 requires no external tools.** It reads the local index built by
`inkentry init`; `search` in an uninitialised directory funnels there rather
than scanning the working tree.

---

## Configuration

### New unified field: `server_url`

Add `server_url` to `Config` as the single entry point for all server-mediated
features.

```toml
# ~/.config/inkentry/config.toml  (personal — never commit)
server_url = "https://inkentry.internal.example.com"
# No credential here: neither config file has a `server_key` field. Store the
# bearer with `inkentry auth set-key --server <url>`, or set
# INKENTRY_SERVER_KEY (ADR-088).

# .inkentry/config.toml  (project-level — safe to commit if key is in env)
project_id = "acme/my-app"
server_url = "https://inkentry.internal.example.com"   # key via INKENTRY_SERVER_KEY env var
```

> `server_url` must be `https://` unless it resolves to loopback
> (`127.0.0.1` / `::1` / `localhost`); a non-loopback `http://` value is
> rejected at config-load time with no override, since the bearer token is
> attached to every server-mediated request. See
> [Server setup → Trust model](../server-setup.md#trust-model).

Environment variable overrides:

| Field | Env var |
|---|---|
| `server_url` | `INKENTRY_SERVER_URL` |
| `project_id` | `INKENTRY_PROJECT_ID` |

`INKENTRY_SERVER_KEY` carries the bearer credential, but it overrides no field:
there is no `server_key` field in either file for it to override (ADR-088).

### Validation

`server_url` present without `project_id` → hard error at load time.

---

## Capability probe

The probe runs **lazily** — not at CLI startup, but on the first command that
needs Tier 1. Once the result is known it is cached for the process lifetime
(no repeated probes).

Algorithm:

```
fn probe_server(cfg: &Config) -> Tier {
    let Some(url) = cfg.server_url else { return Tier::Offline };
    match GET {url}/v1/health within 2s timeout {
        Ok(200, body) => {
            let caps = body["capabilities"].as_array();
            Tier::Server { capabilities: caps }
        }
        _ => {
            warn!("inkentry-server at {url} unreachable — running in offline mode");
            Tier::Offline
        }
    }
}
```

The `capabilities` field in the health response (see server-api.md) allows the
CLI to degrade gracefully if an older server version is deployed that lacks
newer endpoints.

---

## Loopback auto-discovery

**Issue:** #303

In v0.8.0 the common case is no `server_url` at all: the CLI discovers (or
starts) a **local** server on the loopback address. This is what makes Tier 1
the default for a fresh single-user install — semantic search works out of the
box without the user configuring or managing a server.

Discovery runs before the configured-`server_url` probe and only on loopback:

```
fn discover_local_server() -> Option<ServerHandle> {
    if env::var("INKENTRY_NO_SERVER").is_ok() { return None; }   // hard opt-out

    // 1. Probe the well-known loopback endpoint.
    match GET http://127.0.0.1:4655/v1/health within 250ms {
        Ok(200, body) => {
            // 2. Only reuse a server this user owns.
            if body["started_by"] == current_uid() {
                return Some(ServerHandle::existing(body["instance_id"]));
            }
            // Owned by another UID — do not reuse; fall through to no-server.
            warn!("server on 127.0.0.1:4655 started by another user — not reusing");
            return None;
        }
        _ => {}
    }

    // 3. Nothing reachable — autostart the bundled server in the background.
    Some(ServerHandle::spawn_bundled())
}
```

Key points:

- **Address.** Discovery is fixed to `127.0.0.1:4655` — loopback only, never a
  routable interface. A team/remote server is reached through explicit
  `server_url` config, not discovery.
- **`instance_id`.** Each running server reports a unique UUID v7 in its
  `/v1/health` body. The CLI logs it at debug level and uses it to detect
  a server that was restarted underneath a session. Implemented in both server
  and CLI (shipped with PRs #329/#333).
- **`started_by` (UID check).** The health body includes the effective UID of
  the process that started the server. The CLI warns (but does not block) when
  the server was started by a different user — a security hint on shared
  machines. Implemented in both server and CLI (shipped with PRs #329/#333).
- **Autostart.** If nothing is reachable, the CLI spawns the bundled
  `inkentry-server` as a background child owned by the current user, then waits
  for its health endpoint before proceeding.
- **`INKENTRY_NO_SERVER`.** When set, discovery is skipped entirely: no probe, no
  autostart. The CLI runs in Tier 0 and inference-only commands exit 1 with the
  locked-feature message.

<!-- The discovery timeout (250 ms) and autostart/handshake UX are confirmed
     against capability/probe.rs. `instance_id` and `started_by` are implemented
     (PRs #329/#333). -->

User-facing behaviour for these tiers is documented in
[getting-started.md → Capability tiers](../getting-started.md#capability-tiers-where-inference-and-memory-live).

---

## inkentry status — capability section

`inkentry status` gains a capability section above the index stats.

**Text output (Tier 0 — offline):**

```
Capability tier:  Offline
  search          text  [set server_url to enable semantic search]
  memory          sqlite (local)
```

The `memory` line reflects the resolved backend (`sqlite` / `git-notes` /
`remote`), not the capability tier. In a directory with no local `.inkentry/`
project, `inkentry status` reports `No inkentry project here` instead (see
[fail-closed, ADR-067](../adr/067-fail-closed-no-local-project.md)).

**Text output (Tier 1 — server connected):**

```
Capability tier:  Server  (https://inkentry.internal.example.com)
  search          text + semantic
  embedder        ready
  memory          sqlite (local)
```

The `embedder` line reports the server's `embedder.state` from `/v1/health`; it
is omitted when the server does not report that field.

**JSON output** (`inkentry status --format json`) adds a `capabilities` object
(other fields omitted):

```json
{
  "tier": "server",
  "server_url": "https://inkentry.internal.example.com",
  "capabilities": {
    "index_embed": true,
    "memory_harvest": true,
    "memory_pull": true,
    "memory_push": true,
    "memory_search": true,
    "search_semantic": true
  }
}
```

---

## Error messages for locked features

When a Tier 1 feature is invoked but no server is reachable, the command exits
1. Two deliberate message formats are used, selected by which command was run;
both are written to stderr with `eprintln!` (never a panic).

The `require_tier1` commands (`sync`, `plumbing push`, `plumbing pull`) point
the user at `server_url`:

```
Error: 'inkentry sync' requires inkentry-server.
Set server_url in ~/.config/inkentry/config.toml to enable this feature.
       (Tried: https://inkentry.internal.example.com — connection refused)
```

The `(Tried: ...)` line is appended only when a `server_url` is configured but
unreachable. If `server_url` is not set at all it is omitted:

```
Error: 'inkentry sync' requires inkentry-server.
Set server_url in ~/.config/inkentry/config.toml to enable this feature.
```

`harvest` points the user at the local server instead, and also exits 1:

```
Error: 'inkentry harvest' requires inkentry-server.
Run `inkentry server start` to enable this feature.
```

`search` is not gated this way. It degrades to full-text and says so on stderr,
because a text answer over the index is still a useful answer:

```
[no server running — start one with `inkentry server start` to enable semantic ranking; using full-text search]
```

---

## inkentry index — two-phase behaviour

### Phase 1 (always, Tier 0 and Tier 1)

Parse files → produce chunks → extract AST graph edges → store in local DB.
No embeddings generated. Full-text search over the parsed chunks is available as
soon as this phase completes.

### Phase 2 (Tier 1 only)

After Phase 1 completes, if a server is reachable:

1. Collect all chunks that lack an embedding in the local DB.
2. Batch-send to `POST /v1/projects/{id}/index/embed` (see server-api.md).
3. Server returns vectors for each chunk.
4. CLI writes vectors into local DB.
5. Server discards the vectors — it is a processing endpoint, not storage.

The two phases are independent. A partial Phase 2 (network failure mid-batch)
is safe: chunks without embeddings remain in the DB and will be embedded on
the next `inkentry index` run. Phase 1 is never re-run for unchanged files
(blake3 hash check is unaffected).

Phase 1 itself is also crash-safe. The content-hash write and the chunk
writes are not spanned by one transaction, so a kill between them can leave a
file recorded as hash-current with zero chunks. `Database::file_has_chunks`
(`storage/files.rs`) makes the skip check require actual stored chunks, not
just a matching hash, so the next plain run detects that half-indexed state
and reprocesses the file instead of skipping it forever.

The whole `inkentry index` process, both phases, is serialized per project by
a cross-process advisory lock (`cli/cmd/index/run_lock.rs`), taken as the
first thing a run does and released on process exit. A second run that finds the lock held exits immediately with a clean error
rather than racing the first run's writes, which would interleave writes and
corrupt `index.db`.

Progress output during Phase 2:

```
Embedding chunks via server... 1 024 / 3 812  [====>     ] 27%
```

---

## Memory corpus of `search` (semantic ranking is Tier 1 only)

`inkentry search "<query>"` (and `--only-memory`) sends the query text to the
server, which encodes it and returns the vector; the CLI then runs KNN over the
project's local `memory.db`. Note text never leaves the local store. When the
memory backend is an explicit team `server_url`, the server runs the KNN over
its own memory DB instead. `SearchRequest` is `{ query, limit }`; see
server-api.md for the full schema.
