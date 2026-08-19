# Server setup

`inkentry-server` does two jobs. Most users only ever meet the first one:

1. **Local inference server (automatic).** It provides embeddings and LLM
   inference for `inkentry` on your own machine. The CLI starts a local instance
   for you in the background; there is nothing to set up. See
   [Getting started](getting-started.md) for that path; it needs nothing on
   this page.
2. **Team memory server (optional, deployed).** The same binary, run as a
   long-lived service, lets a team share project memory (decisions, context,
   requirements) without sharing code. Each developer's code index stays local;
   only memory entries travel to the server. **This page is about that second
   job.**

If you just installed inkentry and want it to work, you don't need this page:
see [Getting started](getting-started.md) instead.

---

## Team server

Running `inkentry-server` as a **deployed, shared** service so a team can sync
memory is distinct from the local-auto server: it's long-lived, reachable over
the network, and protected by an API key.

**Recommended: bare-metal + systemd.** Run the binary directly on a host under
systemd, bound to a routable interface (`--host 0.0.0.0`) with a certificate and
key (`--tls-cert`/`--tls-key`) and an API key. `inkentry-server` terminates HTTPS
itself (ADR-066), so nothing sits in front of it. Off-host reachability is the
server's own TLS listener, not a separate component. A non-loopback bind is
refused unless both TLS and a key are set (see
[Non-loopback plaintext binds are refused](#non-loopback-plaintext-binds-are-refused-no-override)
below), so there is no way to expose it in cleartext.

**Docker is an equally valid vehicle for the same shape.** With in-process TLS
the container binds its routable interface directly and `-p 443:4658` publishes a
working `https://` endpoint; see [Docker](#4-docker-a-team-server-or-a-local-scaffold)
below.

If you operate your own `inkentry-server` and want remote agents (on a VM, a
k8s pod, or a teammate's laptop across a VPN) to reach it, you are in the
**self-hosted remote (R3)** shape. R3 is just [R1](remote-agents.md) over a
network: the same CLI, the same env vars, the same API. The only thing the
network adds is that TLS becomes mandatory, and the server provides it.

> **Trust model.** Everything that holds the shared `INKENTRY_SERVER_KEY` is a
> full administrator of every project on this server instance; there is no
> per-project access control. See [Trust model](#trust-model) and
> [ADR-056](adr/056-oss-server-tenancy-model.md) before sharing a key with more
> than one mutually-trusting group. If you need isolation between teams, run
> separate server instances (separate keys), not one shared instance.

> Inkentry does not run your agents. This page is about exposing the *memory +
> retrieval* server so remote agents can use it as a peer. It is not an agent
> runtime.

## 1. Get a certificate

`inkentry-server` loads an operator-provided PEM certificate chain and private
key. It does **not** obtain or renew certificates itself (no ACME/Let's Encrypt
automation): you bring a certificate from wherever you already get one, and you
renew it. A certificate with no renewal will eventually expire and the server
will stop answering, so treat renewal as part of running the service.

Any of these are fine:

- an internal CA your fleet already trusts,
- a self-signed or internal-CA certificate you mint yourself (recipe below;
  the common case for a corp-firewall deployment with no public CA reachable),
- `certbot` (or another ACME client) run out-of-band to produce the PEM files,
- a cloud-issued certificate.

You need two files:

- a **certificate chain** (leaf plus any intermediates), PEM, which is public, and
- a **private key**, PEM, which is a high-value secret: keep it `0600` and
  root-owned, and never place it in an environment variable.

### Minting a self-signed / internal-CA certificate

For a deployment with no public CA reachable (the usual corp-firewall case),
generate a self-signed leaf certificate:

```bash
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout tls-key.pem -out tls-cert.pem -days 365 \
  -subj "/CN=inkentry-team" \
  -addext "basicConstraints=critical,CA:FALSE" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=serverAuth" \
  -addext "subjectAltName=DNS:inkentry.internal.example,DNS:localhost,IP:127.0.0.1"
```

Two details in this recipe are load-bearing, not stylistic:

- **`basicConstraints=critical,CA:FALSE` is mandatory.** A bare
  `openssl req -x509 -subj "/CN=..."` with no explicit `basicConstraints`
  produces a `CA:TRUE` certificate. rustls (the CLI's TLS stack) rejects a
  `CA:TRUE` certificate presented as a server's own leaf with
  `CaUsedAsEndEntity`, even when the client is told to trust it: a CA:TRUE
  cert is a certificate *authority* shape, and a server's leaf must be an
  end-entity certificate instead. Verified against the real client: the exact
  same cert without this extension fails with `invalid peer certificate:
  Other(OtherError(CaUsedAsEndEntity))`; with it, the handshake succeeds.
- **The `subjectAltName` list must contain every name and IP clients will use
  in `server_url`.** rustls validates SANs only; it does not fall back to the
  legacy `CN` field the way some older stacks do. A cert whose SAN list is
  missing a hostname or IP a client actually connects through fails validation
  for that client even though the same cert works fine for a client using a
  name that *is* listed. Add every hostname, alias, and IP you expect to
  appear in any client's `server_url`, comma-separated in one `subjectAltName`
  extension (as above), before you distribute the cert.

Distribute `tls-cert.pem` to `--tls-cert` (or the systemd/Docker steps below)
and to clients as `INKENTRY_SERVER_CA` (or `server_ca` in `.inkentry/config.toml`);
see [Trusting the server's certificate on the client](#trusting-the-servers-certificate-on-the-client).
Keep `tls-key.pem` private (`0600`, root-owned); it never leaves the server.

## 2. Run inkentry-server with a routable TLS bind

Bind a routable interface and pass the cert, the key, and an API key. The server
terminates HTTPS itself:

```bash
INKENTRY_SERVER_KEY=$(openssl rand -hex 32) \
inkentry-server \
  --host 0.0.0.0 --port 4658 \
  --tls-cert /etc/inkentry/tls-cert \
  --tls-key  /etc/inkentry/tls-key
```

- `--host 0.0.0.0` (or a specific routable IP) makes the server reachable
  off-host. Loopback (`127.0.0.1`, the default) stays plain-HTTP local-only.
- `--tls-cert` / `--tls-key` are the PEM certificate chain and private key. Both
  or neither: setting one without the other is a startup error. They can also be
  supplied as `INKENTRY_SERVER_TLS_CERT` / `INKENTRY_SERVER_TLS_KEY`.
- An API key is required for any non-loopback bind (`--key` / `--key-file` /
  `INKENTRY_SERVER_KEY`). A routable bind with TLS but no key is refused, as is a
  routable bind with no TLS.

That is the whole exposure story: `https://<host>:4658` now answers, the bearer
key is required, and the connection is encrypted by the server with nothing in
front of it. The bind flags are distinct from the API-key flags on purpose:
`--tls-key` is the TLS private key, `--key`/`--key-file` is the bearer API key,
two different secrets.

## 3. Run it under systemd

Inkentry ships a first-party unit for the team server:
[`packaging/inkentry-server-team.service`](../packaging/inkentry-server-team.service).
It runs the server as a dedicated unprivileged `inkentry` user, binds a routable
interface with TLS, supplies **both** the API key and the TLS private key as
**systemd credentials** rather than environment lines, and applies standard
sandboxing. (This is the deployed team-server unit, distinct from the
per-developer local-inference user unit, `packaging/inkentry-server.service`.)

### Provision the key, the certificate, and the data dir

Two secrets go through systemd's `LoadCredential=`, which reads a `root:root
0600` file and exposes it to the process under `$CREDENTIALS_DIRECTORY` (kept out
of `systemctl show` / `/proc/<pid>/environ`, where an `Environment=` line would
leak it to any local user):

- `server-key` – the bearer API key, read automatically from
  `$CREDENTIALS_DIRECTORY/server-key`.
- `tls-key` – the TLS private key, passed to `--tls-key` via `%d`
  (`%d` = `$CREDENTIALS_DIRECTORY`).

The **certificate chain** is public, so it stays a plain readable path (add it to
`ReadOnlyPaths=` if `ProtectSystem=strict` hides it):

```bash
# Dedicated user + data dir
sudo useradd --system --home-dir /var/lib/inkentry --shell /usr/sbin/nologin inkentry
sudo install -d -o inkentry -g inkentry -m 0750 /var/lib/inkentry

# The bearer key, as a root-only 0600 file
sudo install -d -m 0755 /etc/inkentry
openssl rand -hex 32 | sudo tee /etc/inkentry/server-key >/dev/null
sudo chmod 0600 /etc/inkentry/server-key

# Bring your own TLS cert chain + private key (fullchain/privkey from a real
# CA, or tls-cert.pem/tls-key.pem from the self-signed recipe above)
sudo install -m 0644 fullchain.pem /etc/inkentry/tls-cert   # public chain
sudo install -m 0600 privkey.pem   /etc/inkentry/tls-key    # root:root, private

# Install and start the unit
sudo install -m 0644 packaging/inkentry-server-team.service \
     /etc/systemd/system/inkentry-server.service
sudo systemctl daemon-reload
sudo systemctl enable --now inkentry-server
sudo systemctl status inkentry-server
```

The shipped unit's `ExecStart` is:

```ini
ExecStart=/usr/local/bin/inkentry-server \
  --host 0.0.0.0 --port 4658 \
  --db /var/lib/inkentry/inkentry.db \
  --tls-cert /etc/inkentry/tls-cert \
  --tls-key %d/tls-key
```

with both secrets loaded as credentials:

```ini
LoadCredential=server-key:/etc/inkentry/server-key
LoadCredential=tls-key:/etc/inkentry/tls-key
```

The full unit (dedicated user, hardening directives, and all) is in the repo at
[`packaging/inkentry-server-team.service`](../packaging/inkentry-server-team.service);
install it verbatim as above.

> `MemoryDenyWriteExecute=true` is a further hardening step, but the native
> embedder mmaps model weights and some backends (CUDA/BLAS) may need
> writable-executable pages: validate it against your embedder backend before
> enabling. The shipped unit leaves it commented.

**Key sources.** The credential file is preferred under systemd, but it is not
the only supported source: `INKENTRY_SERVER_KEY` remains a fully-supported equal
alternative (e.g. an `EnvironmentFile=` pointing at a `0600` root-owned file, or
set by tooling that runs the binary outside systemd). Resolution precedence is
`--key` → `--key-file` → `INKENTRY_SERVER_KEY` → `$CREDENTIALS_DIRECTORY/server-key`.
To rotate the bearer key, replace `/etc/inkentry/server-key`, `sudo systemctl
restart inkentry-server`, and redistribute it to clients. To renew the
certificate, replace `/etc/inkentry/tls-cert` (and `/etc/inkentry/tls-key` if the
key changed) and restart the service.

### Alternative: `DynamicUser=`

If you'd rather not manage a static user and data dir, use the
[`DynamicUser=` variant](../packaging/inkentry-server-team-dynamicuser.service):
systemd allocates a per-boot UID and creates `/var/lib/inkentry` via
`StateDirectory=`, chowned to that UID, so there is no `useradd` and no manual
`install -d`. Install it in place of the static-user unit; the key, certificate,
and `/etc/inkentry` provisioning above still applies:

```bash
sudo install -d -m 0755 /etc/inkentry
openssl rand -hex 32 | sudo tee /etc/inkentry/server-key >/dev/null
sudo chmod 0600 /etc/inkentry/server-key
sudo install -m 0644 fullchain.pem /etc/inkentry/tls-cert
sudo install -m 0600 privkey.pem   /etc/inkentry/tls-key
sudo install -m 0644 packaging/inkentry-server-team-dynamicuser.service \
     /etc/systemd/system/inkentry-server.service
sudo systemctl daemon-reload
sudo systemctl enable --now inkentry-server
```

The trade-off: the data dir's owner changes across boots, so any out-of-band
backup tooling must not assume a fixed UID. Prefer the static-user default when
you want a stable owner for backup/inspection or a stable `started_by` UID.

## 4. Docker: a team server or a local scaffold

With in-process TLS, a container is a real team-server vehicle: bind the
container's routable interface, mount a certificate and key, publish the port,
and set an API key. **`docker compose` is the recommended path**:
[`docker-compose.yml`](../docker-compose.yml) already ships a `team-server`
profile that wires this up declaratively, so it is the primary walkthrough
below; a bare `docker run` invocation follows as a manual alternative for
readers not using Compose.

### Recommended: `docker compose --profile team-server`

```bash
git clone https://github.com/inkentries/inkentry
cd inkentry

INKENTRY_SERVER_KEY=$(openssl rand -hex 32) \
INKENTRY_TLS_CERT=/etc/inkentry/tls-cert INKENTRY_TLS_KEY=/etc/inkentry/tls-key \
docker compose --profile team-server up -d
```

`docker compose`'s `pull_policy: build` builds the image for you, no separate
`docker build` step needed. The `team-server` service mounts the cert/key from
the host paths above, publishes the container's routable TLS port on the
host's `443`, and refuses to bind without `INKENTRY_SERVER_KEY` set (ADR-066).
`https://<host>` now answers, keyed, with the container serving TLS itself.

The `team-server` service runs with `restart: unless-stopped` (not
auto-removed), so if it isn't answering, `docker compose ps` shows its state
and `docker compose logs team-server` shows why, including an ADR-066
bind/TLS refusal.

### Manual alternative: bare `docker run`

```bash
git clone https://github.com/inkentries/inkentry
cd inkentry

# Build the image first: there is no published registry tag to pull from,
# so a bare `docker run inkentry-server` with no prior build fails with
# "Unable to find image 'inkentry-server:latest' locally".
docker build -t inkentry-server .

export INKENTRY_SERVER_KEY=$(openssl rand -hex 32)

# Team server: routable TLS bind, cert + key mounted, port published.
# No --rm here: this is a long-lived server, and --rm deletes the container
# (and its only diagnostic output) the moment it exits, including on an
# ADR-066 fail-fast bind/TLS refusal.
docker run -d --name inkentry-server \
  -p 443:4658 \
  -v inkentry-data:/data \
  -v /etc/inkentry/tls-cert:/tls/cert:ro \
  -v /etc/inkentry/tls-key:/tls/key:ro \
  -e INKENTRY_SERVER_KEY \
  -e INKENTRY_SERVER_TLS_CERT=/tls/cert \
  -e INKENTRY_SERVER_TLS_KEY=/tls/key \
  inkentry-server --host 0.0.0.0 --port 4658
```

`https://<host>` now answers, keyed, with the container serving TLS itself.

A `docker run -d` prints a container ID immediately whether or not the server
actually stays up, so an ID alone is not confirmation it is serving. If
nothing answers, or `docker ps` no longer lists it, run `docker logs
inkentry-server` to see why (for example, an ADR-066 bind/TLS refusal).

`docker-compose.yml`'s **default** service is still a **local scaffold**: it
builds the image and runs `inkentry-server` on loopback with a persistent named
volume and no published port, for poking at the API by hand. That default binds
`127.0.0.1` inside the container's own network namespace, so it is reachable only
from inside that namespace (e.g. a sidecar started with `--network
container:inkentry-server`). The runtime image is a minimal Debian base with no
`curl`/`wget`, so the practical way to reach the scaffold is a sidecar:

```bash
docker run --rm --network container:inkentry-server curlimages/curl \
  curl http://127.0.0.1:4655/v1/health
```

To make it team-reachable, give it a routable TLS bind as shown above (the
`team-server` compose profile does this); a bare `docker run -p 4655:4655 ...` of
the loopback scaffold will **not** be reachable, because `-p` forwards host
traffic to the container's routable interface, not into its private loopback, so
nothing published reaches a loopback-only bind.

**What's on the `/data` volume.** Both the SQLite database (`/data/inkentry.db`)
and the native embedder's downloaded model cache (`/data/inkentry/models/`, a
one-time ~339 MB pull) live on the same named volume. Size it accordingly, and
when backing it up, only the database needs your normal database backup
process (per [Production deployment](#production-deployment) below); the model
cache is a re-downloadable artifact, not project data.

## 5. Point a remote agent at it

On the remote host (or in its container), the configuration is identical to
[R1](remote-agents.md); only the URL and the mandatory key change:

```bash
export INKENTRY_SERVER_URL=https://inkentry.example.com
export INKENTRY_SERVER_KEY=your-shared-api-key

inkentry status                # should report the Server capability tier (server reachable over TLS)
inkentry search "auth tokens"
```

The `https://` URL points straight at the server's own TLS listener. The agent's
network path to `inkentry.example.com` is yours to provide: a VPN, Tailscale, or a
public DNS record. Inkentry does not tunnel traffic; it just needs the URL to
resolve and the server to answer.

## Client configuration

Add a `.inkentry/config.toml` at the project root and commit it (it contains no
secrets): one person on the team does this once per repo, and every other
developer picks it up on their next `git pull`, no per-developer setup step for
this file.

```toml
# .inkentry/config.toml: commit this, it's not a secret
server_url = "https://inkentry.internal.example.com"
project_id = "my-awesome-app"
```

Each developer then supplies only the key, which is the actual per-developer
secret (via the environment variable or personal config file below, or the OS
keychain).

> **`server_url` must be `https://` unless it points at loopback**
> (`127.0.0.1` / `::1` / `localhost`). The CLI attaches your bearer token to
> requests built from this URL, so a non-loopback `http://` config is rejected
> at startup with no override. A deployed server serves that `https://` itself,
> so this is satisfied by pointing at its TLS endpoint. Loopback `http://`
> (e.g. while developing against a server on your own machine) is fine.

Personal credential: set it with `inkentry auth set-key`, not a config file:

```bash
inkentry auth set-key --server https://inkentry.internal.example.com
```

The key is read from stdin (piped, or an interactive prompt if you're on a
terminal) and is never accepted as a command argument, so it never lands in
shell history or `ps` output. It's stored in your OS secret store (Keychain /
Secret Service / Credential Manager), keyed by the server's *origin*: see
[ADR-071](adr/071-per-server-client-bearer-scoping.md). That means a
developer working on two projects, each pointing at a different self-hosted
server (the topology [ADR-056](adr/056-oss-server-tenancy-model.md)
recommends over multi-tenancy), holds both keys at once with no collision and
no env-var juggling between them.

Check what's stored with `inkentry auth list-servers` (origins only, never key
material):

```
$ inkentry auth list-servers
https://inkentry.internal.example.com
```

For CI / headless use, the environment variable still works and takes
precedence over any stored key:

```bash
export INKENTRY_SERVER_KEY=your-shared-api-key
```

`project_id` is a human-readable slug, and it goes on the wire exactly as you
wrote it. Both a self-hosted inkentry-server and the hosted cloud API accept
either a slug or a UUID as the project key, so nothing is looked up or cached:
whatever is in `project_id` is what the server sees. (See
[ADR-005](adr/005-cli-slug-uuid-resolution.md) for the resolution step this
replaced.)

> **A key that reached a config file in plaintext is compromised.** Keys live in
> the secret store: the personal `~/.config/inkentry/config.toml` never holds one
> in plaintext, and a committed project `.inkentry/config.toml` ignores a
> `server_key` line entirely. If a key was written to either file, especially if
> it reached git history, treat it as compromised: issue a new key on the server (e.g.
> `openssl rand -hex 32` for a self-managed instance) and run `inkentry auth
> set-key --server <url>` with the new value on every machine that had the old
> Nothing migrates a key out of a config file for you: the line is named on
> stderr and left where it is, so rotating and re-running `set-key` is the whole
> recovery. A key stored by a client older than the per-origin scheme is not
> picked up either; `auth list-servers` shows exactly which origins have a key,
> and an origin missing from that list gets no bearer.

By default a configured `server_url` runs in `local_first` mode: reads and
writes stay in each developer's local `memory.db` and the server is a
converging replica kept in step by a background reconciler (`inkentry status`
shows the active mode; run `inkentry sync` only when you want to force a
synchronous reconcile). Add `mode = "cloud_first"` to the same config to
make the server authoritative for reads and writes; an unreachable server is
then a hard error rather than a silent local read. See [Team server and sync
modes](memory.md#team-server-and-sync-modes).

### Trusting the server's certificate on the client

When the server's certificate chains to a public CA, agents need no extra
configuration. When it is signed by a self-signed or internal CA, point the CLI
at the CA bundle explicitly with the `INKENTRY_SERVER_CA` environment variable:

```bash
export INKENTRY_SERVER_CA=/etc/inkentry/internal-ca.pem   # PEM CA bundle, or the
                                                          # self-signed leaf itself
```

or set it per project in `.inkentry/config.toml`:

```toml
server_ca = "/etc/inkentry/internal-ca.pem"
```

`INKENTRY_SERVER_CA` overrides the config value. The bundle is added as a trust
anchor on top of the built-in roots. TLS verification stays on; there is no
option to disable it.

If you used the self-signed recipe above (a single `CA:FALSE` leaf, not a
separate CA), point `INKENTRY_SERVER_CA` at that same `tls-cert.pem`. If instead
you generated a real internal CA and issued the server a leaf from it,
distribute the **CA** certificate here, not the server's leaf: a certificate
made with a plain `openssl req -x509` and no `basicConstraints` is a self-signed
CA certificate (`CA:TRUE`), and rustls rejects it if the server presents it as
its own end-entity certificate, even when you trust it here.

## Migrating existing local memory

If team members have existing local `memory.db` entries, push them to the server:

```bash
# Make sure .inkentry/config.toml is set up first, then:
inkentry plumbing push
```

This reads your local `memory.db` and sends all active entries to the server.
Archived entries are skipped by default; pass `--include-archived` to push them.

## Multiple projects

One server instance supports multiple projects. Each project has its own
*namespace*: entries from `project_id = "api"` are not mixed with entries
from `project_id = "frontend"`. This is an addressing convenience, **not an
access-control boundary**: see [Trust model](#trust-model) below.

Projects are auto-created on first write; no registration step required.

`GET /v1/projects` enumerates every project slug on the instance. This is
intended behaviour, by design; it is not a data leak to be fixed, it follows
directly from the trust model below.

## Trust model

**A `inkentry-server` instance is a single trust domain.** The shared API key
(`--key` / `INKENTRY_SERVER_KEY`) is the *only* access boundary the server has.
It answers exactly one question, "does this bearer token match the
configured key?", and nothing more: there is no per-project or per-user
authorization layer. Concretely, holding a server's key grants **full
administrator access to every project on that instance**: list, read, search,
write, supersede, archive, and permanently delete, regardless of which project
slug a request names.

This is a deliberate decision, not an oversight; see
[ADR-056](adr/056-oss-server-tenancy-model.md) for the full rationale. The
project-id in the URL path is an addressing convenience for routing requests
to the right namespace; it was never a security boundary, and this document
says so explicitly so no one has to infer it from behaviour.

**What this means for you:**

- A shared/team server is for **one group that already trusts each other**:
  the same trust you'd extend by giving someone commit access to the repo.
  Don't put memory for two teams or organisations that must not see each
  other's data on one instance.
- **Isolation between teams or projects is achieved by running separate server
  instances**, each with its own key and its own database, not by relying on
  project slugs within one instance. Two groups that must not see each other's
  memory run two servers.
- The server enforces this at startup: binding to a non-loopback address with
  a key configured (a shared/team deployment) logs a prominent warning
  restating exactly this: every keyholder is a full administrator of every
  project on the instance.
- If you need per-project or per-user access control within a single
  instance, this server does not provide it (and is not planned to for
  v1.0; see ADR-056's "Revisit if" clause). The managed cloud product
  provides organization-scoped isolation if you need that instead.

## Production deployment

**Bare-metal / systemd is the recommended way to run a team-reachable
`inkentry-server`.** The server binds a routable interface and terminates HTTPS
itself with `--tls-cert`/`--tls-key` and an API key, so it is reachable off-host
with nothing in front of it. See [step 3 above](#3-run-it-under-systemd) for
the systemd unit and the bring-your-own-certificate steps.

A container works equally well for a team server now that the bind is routable
TLS (see [Docker](#4-docker-a-team-server-or-a-local-scaffold) above). The
`docker-compose.yml` **default** service remains a loopback-only local scaffold,
useful for local development or testing; its `team-server` profile is the
routable-TLS shape.

Key considerations for any deployment:
- Putting the server behind a VPN or private subnet is still good
  defense-in-depth (the API key is the app-level guard; network-level access
  control is an additional layer, not a substitute for it)
- The SQLite WAL-mode database handles 2–20 concurrent writers comfortably;
  that write concurrency is the practical ceiling for a single server
- Back up the database file with your normal database backup process

## Running without Docker

```bash
# Build
cargo build --release --bin inkentry-server

# Check version
./target/release/inkentry-server --version
# inkentry-server 0.9.4

# Run
./target/release/inkentry-server \
  --db /var/lib/inkentry/inkentry.db \
  --port 4655 \
  --key your-api-key
```

### Bind and auth flags

| Flag | Env | Default | Purpose |
|---|---|---|---|
| `--host` | (none) | `127.0.0.1` | Interface to bind. Non-loopback needs both a key and TLS (`--tls-cert`/`--tls-key`); see below. |
| `--port` | (none) | `4655` | Port to bind. |
| `--key` | (none) | unset | Shared bearer API key, passed inline. Visible in the process table; prefer `--key-file` or `INKENTRY_SERVER_KEY`. Leave every key source unset only for a loopback dev server. |
| `--key-file` | (none) | unset | Read the key from a file (whole contents, trimmed). First-class alternative to `INKENTRY_SERVER_KEY`, not a fallback. |
| (none) | `INKENTRY_SERVER_KEY` | unset | Read the key from the environment. Fully supported alongside `--key-file`. |
| `--tls-cert` | `INKENTRY_SERVER_TLS_CERT` | unset | PEM certificate chain (leaf + intermediates) for in-process HTTPS. The chain is public. Set with `--tls-key` (both or neither). Distinct from `--key`/`--key-file`. |
| `--tls-key` | `INKENTRY_SERVER_TLS_KEY` | unset | PEM private key matching `--tls-cert`. A high-value secret: supply via a systemd credential or a `0600` root-owned file, never an `Environment=` line. Set with `--tls-cert`. |

### Operational flags

| Flag | Default | Purpose |
|---|---|---|
| `--health-check` | – | Probe this server's own `/v1/health` on the configured `--host`/`--port`, then exit 0 if live and non-zero otherwise. A wildcard `--host` is probed over loopback. |
| `--embedding-dim <n>` | `896` | Embedding dimension expected from clients. Must match the team's model; `896` is F2LLM-v2-330M. |
| `--conflict-threshold <f>` | `0.92` | Cosine similarity at or above which a new memory entry is treated as conflicting with an existing active one and answered `409`. `1.0` disables conflict detection. |

`--health-check` exists so a container image needs no `curl` or `wget` for its
`HEALTHCHECK`: the binary probes itself.

```dockerfile
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s \
  CMD ["inkentry-server", "--health-check"]
```

Pass the same `--host`/`--port` as the serving process if you have changed them
from the defaults, since the probe reads them to know where to look.

The certificate is bring-your-own PEM (an internal CA, a self-signed cert you
mint yourself, `certbot`, or a cloud-issued cert). `inkentry-server` does not
obtain or renew it (no ACME); the operator renews it.

The key is resolved from, in precedence order: `--key` → `--key-file` →
`INKENTRY_SERVER_KEY` → a systemd `LoadCredential=server-key` (read automatically
from `$CREDENTIALS_DIRECTORY/server-key` when present). A blank value from any
source is ignored and falls through to the next. Under systemd the credential
path is preferred: it keeps the key out of the world-readable process
environment.

> **A `401` does not always mean the key was rejected.** Authentication runs
> before route matching, so a request with a missing or wrong bearer token is
> answered `401 unauthorized` even when the path it names does not exist. A
> typo'd URL and an unusable key look identical on the wire. Re-run the same
> request with a key you are sure of: if the answer turns into `404`, the path
> was wrong, not the credential. Get the real route list from
> `inkentry-server --print-openapi`. See [Auth
> architecture](architecture/server-api.md#auth-architecture).

### Embedding CPU thread budget

On a CPU-only host the bundled native embedder (candle) would otherwise fan a
single embed batch across every core, leaving the server's own request handling
to compete with it for CPU. To leave headroom, the server caps candle's thread
count at startup.

| Env | Default | Purpose |
|---|---|---|
| `INKENTRY_EMBED_THREADS` | see below | CPU threads the native embedder may use. |

The default reserves a quarter of the host, capped at two, from each of two
counts and takes the smaller result: physical cores (embed throughput plateaus
there, because SMT siblings share the vector units a forward pass saturates)
and logical processors (what the OS actually hands the async runtime to
schedule on). Any host with eight or more physical cores gets `physical − 2`, as
before; smaller hosts keep a proportional share instead of surrendering the
machine. A 2-physical/4-logical laptop resolves to 2 threads, not 1.

Precedence: `INKENTRY_EMBED_THREADS` > an already-set `RAYON_NUM_THREADS` >
the bounded default. A pre-set `RAYON_NUM_THREADS` is respected and never
overridden. The resolved value and its source are logged at startup
(`embed CPU thread budget resolved`), and the value is reported as
`limits.embed_threads` on `/v1/health`. When it resolves to 1, both that log
and `inkentry status` name this variable, since a single-threaded first index
is the difference between minutes and hours. GPU (Metal/CUDA) builds are
unaffected.

This budget is not what keeps the server answering while an embedder is busy,
and lowering it will not make a slow probe fast. `/v1/health` and every other
endpoint that does not itself embed never touch the embedder's forward pass, so
they stay responsive for the whole of an index whatever this value is set to.

This budget only bounds CPU contention *within* a single embed batch; embed
requests themselves are still serialized behind a single mutex on both device
paths (GPU concurrency would blow its memory limit, and a CPU batch already
uses most of this budget on its own). A bounded admission queue sits in front of the
embedder: when it is full the server sheds the request immediately with `429`
and a `Retry-After` header, rather than letting a batch queue behind a running
index until the caller's own timeout fires (see
`POST /index/embed` in `architecture/server-api.md`).

### Non-loopback plaintext binds are refused, no override

`inkentry-server` refuses to bind a non-loopback address over plaintext HTTP,
whether or not a key is set, and there is no opt-out. With no key that would be
an open, unauthenticated server; with a key the bearer `INKENTRY_SERVER_KEY`
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
server terminates HTTPS itself. Plaintext off-host stays refused with no
override.

### Client identity and the `/llm/complete` rate limit

`POST /llm/complete` is rate limited per caller (60 requests/minute), which is
what bounds how much of your LLM budget one client can spend. A caller is
identified by its bearer principal **and** its address, so a shared team key
does not collapse everyone onto one bucket.

That address is the **TCP peer address of the connection**, not the
`X-Forwarded-For` header. The header is set by whoever opened the connection, so
a caller that could choose it could mint a fresh budget on every request and the
limit would not exist. Since the server terminates TLS itself (ADR-066) there is
normally nothing in front of it and nothing legitimate to trust.

| Flag | Env | Default | Purpose |
|---|---|---|---|
| `--trusted-proxy` | `INKENTRY_TRUSTED_PROXIES` | unset (empty) | IP address of a reverse proxy whose `X-Forwarded-For` this server should believe. Repeatable; comma-separated in the env var. |

Set it only if you genuinely run a proxy in front of the server:

```bash
inkentry-server --trusted-proxy 10.0.0.5 --trusted-proxy 10.0.0.6 ...
```

With it set, an `X-Forwarded-For` entry is honoured **only** when the request's
TCP peer is one of the listed addresses, and only when that entry parses as an
IP address; anything else falls back to the peer. Naming an address that is not
actually a proxy hands the rate limit to any client that can reach the server
from there.

The entry read is the **last** one in the header, not the first. Both common
proxy configurations are then handled correctly:

- **Appending** (nginx's usual `proxy_set_header X-Forwarded-For
  $proxy_add_x_forwarded_for`) keeps whatever the client sent and adds the
  address the proxy saw. A client sending `X-Forwarded-For: 9.9.9.9` arrives as
  `9.9.9.9, <its real address>`; only the last entry is observed fact, so only
  it is used.
- **Overwriting** (`proxy_set_header X-Forwarded-For $remote_addr`) leaves a
  single entry, where last and first are the same value.

You therefore do not need to change your proxy's header handling. What you do
need is for your proxy to be the server's **immediate peer**: only the address
`--trusted-proxy` names is trusted, and behind a chain of two or more proxies
the last entry is the inner proxy rather than the originating client. That case
still fails safe — the bucket is keyed on a proxy address rather than one the
caller chose — but every client behind that chain shares one budget.

Addresses are compared in canonical form, so on a dual-stack bind (`--host ::`)
a proxy that connects over IPv4 and presents as `::ffff:10.0.0.5` still matches
`--trusted-proxy 10.0.0.5`. Write the plain IPv4 address.

## Air-gapped / no-egress install

`inkentry-server` normally fetches the bundled F2LLM-v2-330M embedder from
Hugging Face Hub the first time it's needed (see [Getting
started](getting-started.md)). On a host with no route to `huggingface.co`,
an air-gapped network, a strict corp firewall, a build image with no egress,
that download has nothing to reach. `--model-dir` (or `INKENTRY_MODEL_DIR`)
points the bundled native embedder at a directory you provisioned out of
band instead, with zero network access at startup or at runtime:

```bash
inkentry-server --model-dir /srv/inkentry/models
# or
export INKENTRY_MODEL_DIR=/srv/inkentry/models
inkentry-server
```

Only consulted when the bundled native embedder is enabled (the
`embed-native` build feature); ignored otherwise.

### Directory layout

`--model-dir` expects a flat directory, no nested subdirectories, containing:

| File | Required | Notes |
|---|---|---|
| `f2llm-v2-330m-q8_0.gguf` | yes | pre-quantized Q8_0 embedder weights |
| `tokenizer.json` | yes | matching tokenizer |
| `config.json` | no | auto-written from an embedded copy if absent; supply it only to override that default |

A missing directory, or a missing GGUF or tokenizer inside it, fails fast
with an error naming the missing piece and pointing back at this section.

### Fetch-and-transfer procedure

Produce that directory on a machine that does have network access, then
carry it to the air-gapped host:

1. On the connected machine, run `inkentry-server` once with no `--model-dir`.
   This populates the normal online cache at `~/.local/share/inkentry/models/`
   (Linux: `$XDG_DATA_HOME/inkentry/models/`), which ends up holding:
   - `f2llm-v2-330m-q8_0.gguf`
   - `config.json`
   - a nested `tokenizer.json`, under
     `models--spelunk-cloud--F2LLM-v2-330M-Q8_0-GGUF/snapshots/<rev>/tokenizer.json`
     (hf-hub's own cache layout)
2. Copy the GGUF and that nested `tokenizer.json` into a new, flat directory.
   `config.json` doesn't need to come along; a missing one is regenerated on
   the air-gapped host.
   ```bash
   mkdir -p offline-model
   cp ~/.local/share/inkentry/models/f2llm-v2-330m-q8_0.gguf offline-model/
   cp ~/.local/share/inkentry/models/models--spelunk-cloud--F2LLM-v2-330M-Q8_0-GGUF/snapshots/*/tokenizer.json \
      offline-model/
   ```
3. Transfer `offline-model/` to the air-gapped host by whatever out-of-band
   means your environment allows (removable media, an internal artifact
   store, etc.), and point `--model-dir` / `INKENTRY_MODEL_DIR` at it there.

### Verifying integrity

Both files come from the first-party `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF`
Hugging Face repo; see [Model attribution](model-attribution.md) for
provenance and license. As fetched at time of writing, their SHA-256 sums are:

| File | SHA-256 |
|---|---|
| `f2llm-v2-330m-q8_0.gguf` | `2c12aad2951f1d9a3b457f890a2586d1ee19b755b377c0fb424e856e615b8f2b` |
| `tokenizer.json` | `7e295e5bb91a3d35335f92fa4294a6e4e0ab4aa586db853e14312a62135bfddc` |

`inkentry-server` fetches from that repo's `main` branch rather than a pinned
commit, so these values track whatever is currently published there. Treat
them as a convenience check, not a permanent guarantee: for integrity
verification on artifacts fetched later, recompute and compare against the
source's own published hash instead of trusting this table indefinitely.

```bash
shasum -a 256 f2llm-v2-330m-q8_0.gguf tokenizer.json
```

Hugging Face also serves each file's hash directly, in the `x-linked-etag`
response header:

```bash
curl -sI https://huggingface.co/spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF/resolve/main/f2llm-v2-330m-q8_0.gguf \
  | grep -i x-linked-etag
```

## Related

- [Server API](architecture/server-api.md): the HTTP + SSE surface the server exposes
- [Third-party models](third-party-models.md): configuring an external LLM or embedding endpoint
- [Model attribution](model-attribution.md): license/provenance for the bundled embedder
- [Remote agents](remote-agents.md): the R1 local-Docker path
- [Getting started](getting-started.md): the zero-setup local path
