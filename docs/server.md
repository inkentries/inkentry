# spelunk-server

`spelunk-server` does two jobs. Most users only ever meet the first one:

1. **Local inference server (automatic).** It provides embeddings and LLM
   inference for `spelunk` on your own machine. As of v0.8.0 the CLI starts a
   local instance for you in the background — there is nothing to set up.
2. **Team memory server (optional, deployed).** The same binary, run as a
   long-lived service, lets a team share project memory (decisions, context,
   requirements) without sharing code. Each developer's code index stays local;
   only memory entries travel to the server.

If you just installed spelunk and want it to work, you want the local-auto
section below and nothing else. The team-server material starts at
[Team server](#team-server).

---

## Local server (automatic — no setup)

When you run a command that needs inference — `spelunk init`, a semantic
`spelunk search`, `spelunk explore` — the CLI looks for a server on the loopback
address `127.0.0.1:7777`. If none is running, it starts the bundled
`spelunk-server` in the background, owned by your user, and reuses it for the
rest of the session and future runs. You don't configure anything, and you don't
manage a process. Memory still lives in the local project's `memory.db`; the
local server only provides inference.

If you ever need to manage it explicitly:

```bash
spelunk server start     # start the local server (no-op if already running)
spelunk server stop      # stop the local server
spelunk server status    # show whether a local server is running and its PID
spelunk server logs      # tail the local server's logs
```

`stop` terminates even a wedged server whose `/v1/health` has stopped
responding: it sends SIGTERM, escalates to SIGKILL after a bounded wait, and
reports success only once the process is confirmed gone. `start` reclaims a
stale or hung prior daemon on the requested port rather than drifting to a
different one, and fails loudly if an unrelated process already holds that port.
A single-instance guard keeps two servers from running against the same
`server.db`.

To opt out entirely and keep spelunk fully offline, set `SPELUNK_NO_SERVER=1`
(see [Capability tiers](getting-started.md#capability-tiers-where-inference-and-memory-live)).
With it set, spelunk never autostarts a server and inference-only features exit
with a clear message instead.

### Windows: allow the local listener through the firewall

On Windows, the first time the local server binds its loopback port, Windows
Defender Firewall may show a prompt — **accept it**. If it's blocked, `spelunk`
can't reach the server and quietly drops to text/ast-grep search (you'll see a
one-line "no server reachable" notice). If you dismissed the prompt, run
`spelunk server start` again to re-trigger it, then `spelunk server status` to
confirm the server is reachable.

How discovery decides whether to reuse or start a server is documented in
[CLI capability tiers → Loopback auto-discovery](architecture/capability-tiers.md#loopback-auto-discovery).
The `instance_id` and `started_by` UID checks described there are implemented
as of v0.8.0 (PRs #329/#333).

---

## Team server

The rest of this page covers running `spelunk-server` as a **deployed, shared**
service so a team can sync memory. This is distinct from the local-auto server
above: it's long-lived, reachable over the network, and protected by an API key.

**Recommended: bare-metal + systemd.** Run the binary directly on a host under
systemd, bound to a routable interface (`--host 0.0.0.0`) with a certificate and
key (`--tls-cert`/`--tls-key`) and an API key. `spelunk-server` terminates HTTPS
itself (ADR-066), so nothing sits in front of it. Off-host reachability is the
server's own TLS listener, not a separate component. A non-loopback bind is
refused unless both TLS and a key are set (see
[Non-loopback plaintext binds are refused](#non-loopback-plaintext-binds-are-refused-no-override)
below), so there is no way to expose it in cleartext.

**Docker is an equally valid vehicle for the same shape.** With in-process TLS
the container binds its routable interface directly and `-p 443:7777` publishes a
working `https://` endpoint; see [Docker](#docker-a-team-server-or-a-local-scaffold)
below.

**[Self-hosting](self-hosting.md) is the full team-server guide** — it walks
through the routable TLS bind, bringing your own certificate, and the first-party
`spelunk-server-team.service` systemd unit (hardened, both the API key and the
TLS private key supplied as systemd credentials). Start there. The rest of this
page covers client configuration, the trust model, and the CLI/flag reference
that path relies on.

## Docker: a team server or a local scaffold

With in-process TLS, a container is a real team-server vehicle. Bind the
container's routable interface, mount a certificate and key, publish the port,
and set an API key:

```bash
git clone https://github.com/spelunk-cloud/spelunk
cd spelunk

export SPELUNK_SERVER_KEY=$(openssl rand -hex 32)

# Team server: routable TLS bind, cert + key mounted, port published.
docker run --rm -d --name spelunk-server \
  -p 443:7777 \
  -v spelunk-data:/data \
  -v /etc/spelunk/tls-cert:/tls/cert:ro \
  -v /etc/spelunk/tls-key:/tls/key:ro \
  -e SPELUNK_SERVER_KEY \
  -e SPELUNK_SERVER_TLS_CERT=/tls/cert \
  -e SPELUNK_SERVER_TLS_KEY=/tls/key \
  spelunk-server --host 0.0.0.0 --port 7777
```

`https://<host>` now answers, keyed, with the container serving TLS itself. The
`team-server` profile in [`docker-compose.yml`](../docker-compose.yml) wires the
same thing up declaratively.

`docker-compose.yml`'s **default** service is still a **local scaffold**: it
builds the image and runs `spelunk-server` on loopback with a persistent named
volume and no published port, for poking at the API by hand. That default binds
`127.0.0.1` inside the container's own network namespace, so it is reachable only
from inside that namespace (e.g. a sidecar started with `--network
container:spelunk-server`). The runtime image is a minimal Debian base with no
`curl`/`wget`, so the practical way to reach the scaffold is a sidecar:

```bash
docker run --rm --network container:spelunk-server curlimages/curl \
  curl http://127.0.0.1:7777/v1/health
```

To make it team-reachable, give it a routable TLS bind as shown above (the
`team-server` compose profile does this); a bare `docker run -p 7777:7777 ...` of
the loopback scaffold will **not** be reachable, because `-p` forwards host
traffic to the container's routable interface, not into its private loopback, so
nothing published reaches a loopback-only bind.

## Client configuration

Each developer adds a `.spelunk/config.toml` at the project root (commit it):

```toml
# .spelunk/config.toml — commit this, it's not a secret
server_url = "https://spelunk.internal.example.com"
project_id = "my-awesome-app"
```

> **`server_url` must be `https://` unless it points at loopback**
> (`127.0.0.1` / `::1` / `localhost`). The CLI attaches your bearer token to
> requests built from this URL, so a non-loopback `http://` config is rejected
> at startup with no override. A deployed server serves that `https://` itself
> (see [Self-hosting](self-hosting.md)), so this is satisfied by pointing at its
> TLS endpoint. Loopback `http://` (e.g. while developing against a server on
> your own machine) is fine.

Personal config (`~/.config/spelunk/config.toml` — never commit):

```toml
# ~/.config/spelunk/config.toml
server_key = "your-shared-api-key"
```

`project_id` is a human-readable slug. If the server routes projects by an
internal UUID (as a team/cloud memory server does), the CLI resolves the slug to
that UUID automatically on first use and caches it in
`.spelunk/cloud-project-id.lock`. You don't need to look the UUID up by hand.
The cache is keyed on the slug, so renaming the project re-resolves it
automatically; set `SPELUNK_NO_SLUG_CACHE=1` to force a fresh lookup. A raw UUID
in `project_id` is used as-is. (See [ADR-005](adr/005-cli-slug-uuid-resolution.md).)

Or use the environment variable:

```bash
export SPELUNK_SERVER_KEY=your-shared-api-key
```

## Migrating existing local memory

If team members have existing local `memory.db` entries, push them to the server:

```bash
# Make sure .spelunk/config.toml is set up first, then:
spelunk memory push
```

This reads your local `memory.db` and sends all active entries to the server.
Archived entries are skipped by default; pass `--include-archived` to push them.

## Multiple projects

One server instance supports multiple projects. Each project has its own
*namespace* — entries from `project_id = "api"` are not mixed with entries
from `project_id = "frontend"`. This is an addressing convenience, **not an
access-control boundary**: see [Trust model](#trust-model) below.

Projects are auto-created on first write — no registration step required.

`GET /v1/projects` enumerates every project slug on the instance. This is
intended behaviour, by design — it is not a data leak to be fixed, it follows
directly from the trust model below.

## Trust model

**A `spelunk-server` instance is a single trust domain.** The shared API key
(`--key` / `SPELUNK_SERVER_KEY`) is the *only* access boundary the server has.
It answers exactly one question — "does this bearer token match the
configured key?" — and nothing more: there is no per-project or per-user
authorization layer. Concretely, holding a server's key grants **full
administrator access to every project on that instance**: list, read, search,
write, supersede, archive, and permanently delete, regardless of which project
slug a request names.

This is a deliberate decision, not an oversight — see
[ADR-056](adr/056-oss-server-tenancy-model.md) for the full rationale. The
project-id in the URL path is an addressing convenience for routing requests
to the right namespace; it was never a security boundary, and this document
says so explicitly so no one has to infer it from behaviour.

**What this means for you:**

- A shared/team server is for **one group that already trusts each other** —
  the same trust you'd extend by giving someone commit access to the repo.
  Don't put memory for two teams or organisations that must not see each
  other's data on one instance.
- **Isolation between teams or projects is achieved by running separate server
  instances** — each with its own key and its own database — not by relying on
  project slugs within one instance. Two groups that must not see each other's
  memory run two servers.
- The server enforces this at startup: binding to a non-loopback address with
  a key configured (a shared/team deployment) logs a prominent warning
  restating exactly this — every keyholder is a full administrator of every
  project on the instance.
- If you need per-project or per-user access control within a single
  instance, this server does not provide it (and is not planned to for
  v1.0 — see ADR-056's "Revisit if" clause). The managed cloud product
  provides organization-scoped isolation if you need that instead.

## Embedding dimension

All clients writing to the same project must use the same embedding model. The
embedding model is fixed product-wide to codefuse-ai/F2LLM-v2-330M (896-dim) and
cannot be selected — a mismatched model silently corrupts semantic search. The
server records the embedding dimension on the first write and rejects subsequent
writes with a different dimension.

Default: 896 dimensions (codefuse-ai/F2LLM-v2-330M, the bundled native embedder).

`--embedding-dim` sets the dimension the server enforces. Change it only to match
an external endpoint whose vectors differ in size — doing so means you are
running a different model at your own risk (the one-model-per-vector-space
invariant no longer holds), not a supported way to swap embedding models:

```bash
docker compose run spelunk-server --embedding-dim 1024
```

Or via compose environment:

```yaml
environment:
  SPELUNK_EMBEDDING_DIM: "1024"
```

## Production deployment

**Bare-metal / systemd is the recommended way to run a team-reachable
`spelunk-server`.** The server binds a routable interface and terminates HTTPS
itself with `--tls-cert`/`--tls-key` and an API key, so it is reachable off-host
with nothing in front of it. See [Self-hosting](self-hosting.md) for the systemd
unit and the bring-your-own-certificate steps.

A container works equally well for a team server now that the bind is routable
TLS (see [Docker](#docker-a-team-server-or-a-local-scaffold) above). The
`docker-compose.yml` **default** service remains a loopback-only local scaffold,
useful for local development or testing; its `team-server` profile is the
routable-TLS shape.

Key considerations for any deployment:
- Putting the server behind a VPN or private subnet is still good
  defense-in-depth (the API key is the app-level guard; network-level access
  control is an additional layer, not a substitute for it)
- The SQLite WAL-mode database handles 2–20 concurrent writers comfortably
- Back up the database file with your normal database backup process
- For large teams or heavy write loads, see the plan for Postgres support

## Running without Docker

```bash
# Build
cargo build --release --bin spelunk-server

# Check version
./target/release/spelunk-server --version
# spelunk-server 0.9.0

# Run
./target/release/spelunk-server \
  --db /var/lib/spelunk/spelunk.db \
  --port 7777 \
  --key your-api-key
```

### Bind and auth flags

| Flag | Env | Default | Purpose |
|---|---|---|---|
| `--host` | (none) | `127.0.0.1` | Interface to bind. Non-loopback needs both a key and TLS (`--tls-cert`/`--tls-key`); see below. |
| `--port` | (none) | `7777` | Port to bind. |
| `--key` | (none) | unset | Shared bearer API key, passed inline. Visible in the process table — prefer `--key-file` or `SPELUNK_SERVER_KEY`. Leave every key source unset only for a loopback dev server. |
| `--key-file` | (none) | unset | Read the key from a file (whole contents, trimmed). First-class alternative to `SPELUNK_SERVER_KEY`, not a fallback. |
| (none) | `SPELUNK_SERVER_KEY` | unset | Read the key from the environment. Fully supported alongside `--key-file`. |
| `--tls-cert` | `SPELUNK_SERVER_TLS_CERT` | unset | PEM certificate chain (leaf + intermediates) for in-process HTTPS. The chain is public. Set with `--tls-key` (both or neither). Distinct from `--key`/`--key-file`. |
| `--tls-key` | `SPELUNK_SERVER_TLS_KEY` | unset | PEM private key matching `--tls-cert`. A high-value secret: supply via a systemd credential or a `0600` root-owned file, never an `Environment=` line. Set with `--tls-cert`. |

The certificate is bring-your-own PEM (an internal CA, `certbot`, or a
cloud-issued cert). `spelunk-server` does not obtain or renew it (no ACME); the
operator renews it. See [Self-hosting](self-hosting.md).

The key is resolved from, in precedence order: `--key` → `--key-file` →
`SPELUNK_SERVER_KEY` → a systemd `LoadCredential=server-key` (read automatically
from `$CREDENTIALS_DIRECTORY/server-key` when present). A blank value from any
source is ignored and falls through to the next. Under systemd the credential
path is preferred — it keeps the key out of the world-readable process
environment; see [Self-hosting](self-hosting.md).

### Embedding CPU thread budget

On a CPU-only host the bundled native embedder (candle) would otherwise fan a
single embed batch across every core, briefly starving the server's own request
handling (`/v1/health` can go unresponsive during a large index). To leave
headroom, the server caps candle's thread count at startup.

| Env | Default | Purpose |
|---|---|---|
| `SPELUNK_EMBED_THREADS` | `max(1, physical cores − 2)` | CPU threads the native embedder may use. Reserves ~2 cores for request serving. |

Precedence: `SPELUNK_EMBED_THREADS` > an already-set `RAYON_NUM_THREADS` >
the bounded default. A pre-set `RAYON_NUM_THREADS` is respected and never
overridden. The resolved value and its source are logged at startup
(`embed CPU thread budget resolved`). GPU (Metal/CUDA) builds are unaffected.

### Non-loopback plaintext binds are refused, no override

`spelunk-server` refuses to bind a non-loopback address over plaintext HTTP,
whether or not a key is set, and there is no opt-out. With no key that would be
an open, unauthenticated server; with a key the bearer `SPELUNK_SERVER_KEY`
would travel across the network in cleartext. The refusal names the
interface/port and points at `--tls-cert`/`--tls-key`.

The rule the server enforces at startup is exactly the local/remote boundary:

| Bind | TLS configured | Key set | Result |
|---|---|---|---|
| loopback | any | any | allow (local HTTP, no key needed) |
| non-loopback | no | any | refuse (no plaintext off-host, keyed or not) |
| non-loopback | yes | no | refuse (remote requires an API key) |
| non-loopback | yes | yes | allow (the remote HTTPS path) |

So the supported way to reach the server from another machine (including a
container) is a routable bind with `--tls-cert`/`--tls-key` and a key, where the
server terminates HTTPS itself (see [Self-hosting](self-hosting.md)). Plaintext
off-host stays refused with no override.

## API reference

All routes require `Authorization: Bearer <key>` except `/v1/health`, which is
unauthenticated by design (it's the liveness probe used before a client knows
whether a key is even needed) — the CLI never attaches a bearer token to it.

```
GET    /v1/health
GET    /v1/projects
POST   /v1/projects/{project_id}/memory
GET    /v1/projects/{project_id}/memory           ?kind=&limit=&archived=
GET    /v1/projects/{project_id}/memory/{id}
POST   /v1/projects/{project_id}/memory/search
DELETE /v1/projects/{project_id}/memory/{id}
POST   /v1/projects/{project_id}/memory/{id}/archive
POST   /v1/projects/{project_id}/memory/{id}/supersede
GET    /v1/projects/{project_id}/memory/since     ?t=<epoch>&limit=
GET    /v1/projects/{project_id}/memory/stream    (Server-Sent Events)
GET    /v1/projects/{project_id}/memory/harvested-shas
GET    /v1/projects/{project_id}/stats
POST   /v1/projects/{project_id}/index/embed      (embedding proxy — vectors not stored)
POST   /v1/projects/{project_id}/search           (query embedding proxy for CLI KNN)
POST   /v1/projects/{project_id}/explore          (SSE — LLM reasoning loop)
POST   /v1/projects/{project_id}/llm/complete     (SSE — raw LLM completion)
```

`POST /index/embed` accepts a JSON batch of chunks (max 256) and returns the
vectors as `application/octet-stream`: raw little-endian `f32` bytes, row-major
`[n_chunks × dim]` (896 with the default embedder), in request order, with no
per-row framing. The client maps response row `i` to request chunk `i` by
position. The server does not store the vectors — the CLI is the only persistent
store for index data. See `docs/openapi.json` for the full schema.

`/index/embed` has its own, much longer request timeout (1800s) than the rest
of the API (30s) — a legitimate batch can genuinely take minutes on slow or
CPU-only hardware. `GET /v1/health`'s `limits` object advertises the current
server's `embed_request_timeout_secs`, `max_batch_chunks`, and (when the native
embedder is loaded) `embedder_token_cap`, so a client can size its own batching
to the server it's actually talking to; a server predating this field should be
assumed to still enforce the old blanket 30s budget with no exemption.

### Conflict detection

When `POST /v1/projects/{project_id}/memory`, the server checks if a semantically similar entry already exists (cosine similarity >= 0.92). If a conflict is detected, the response is **HTTP 409** with a JSON body:

```json
{
  "stored": true,
  "id": 42,
  "conflicts": [
    { "id": 37, "title": "Previous similar entry", "similarity": 0.97 }
  ]
}
```

The new entry is stored with a `contradicts` edge to the conflicting entry. Clients should log or display this warning. Configure the threshold with `--conflict-threshold` flag (0.0–1.0, default 0.92).

### Polling for new entries

Use `GET /memory/since?t=<epoch>&limit=N` to retrieve entries created after a Unix timestamp:

```bash
spelunk memory since 1700000000
```

Returns up to N entries (default 50) created after the given epoch, sorted ascending by creation time.

### Streaming entries

Use `GET /memory/stream` (Server-Sent Events) to subscribe to new entries as they arrive:

```bash
spelunk memory watch
```

Each line is a JSON object representing a newly added entry. The stream persists until the client disconnects.
