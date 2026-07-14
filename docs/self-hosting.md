# Self-hosting spelunk-server (R3)

This is the **recommended way to run a shared team `spelunk-server`**: the binary
on a host under systemd, bound to a routable interface, terminating HTTPS itself
with a certificate and key you provide. `spelunk-server` serves TLS in-process
(ADR-066), so there is nothing in front of it: no separate terminator to install,
own, or keep current. A container is an equally valid vehicle for the same shape
(see [Server setup → Docker](server.md#docker-a-team-server-or-a-local-scaffold)).

If you operate your own `spelunk-server` and want remote agents – on a VM, a k8s
pod, or a teammate's laptop across a VPN – to reach it, you are in the
**self-hosted remote (R3)** shape. R3 is just [R1](remote-agents.md) over a
network: the same CLI, the same env vars, the same API. The only thing the
network adds is that **TLS becomes mandatory**, and the server provides it.

`spelunk-server` speaks plain HTTP on loopback and HTTPS off it. A non-loopback
bind is refused unless you pass both TLS flags **and** an API key, so there is no
way to accidentally expose it in cleartext.

> **Trust model.** Everything that holds the shared `SPELUNK_SERVER_KEY` is a
> full administrator of every project on this server instance; there is no
> per-project access control. See [Trust model](server.md#trust-model) and
> [ADR-056](adr/056-oss-server-tenancy-model.md) before sharing a key with more
> than one mutually-trusting group. If you need isolation between teams, run
> separate server instances (separate keys), not one shared instance.

> Spelunk does not run your agents. This page is about exposing the *memory +
> retrieval* server so remote agents can use it as a peer. It is not an agent
> runtime.

## 1. Bring your own certificate

`spelunk-server` loads an operator-provided PEM certificate chain and private
key. It does **not** obtain or renew certificates itself (no ACME/Let's Encrypt
automation): you bring a certificate from wherever you already get one, and you
renew it. A certificate with no renewal will eventually expire and the server
will stop answering, so treat renewal as part of running the service.

Any of these are fine:

- an internal CA your fleet already trusts,
- `certbot` (or another ACME client) run out-of-band to produce the PEM files,
- a cloud-issued certificate.

You need two files:

- a **certificate chain** (leaf plus any intermediates), PEM, which is public, and
- a **private key**, PEM, which is a high-value secret: keep it `0600` and
  root-owned, and never place it in an environment variable.

## 2. Run spelunk-server with a routable TLS bind

Bind a routable interface and pass the cert, the key, and an API key. The server
terminates HTTPS itself:

```bash
SPELUNK_SERVER_KEY=$(openssl rand -hex 32) \
spelunk-server \
  --host 0.0.0.0 --port 7777 \
  --tls-cert /etc/spelunk/tls-cert \
  --tls-key  /etc/spelunk/tls-key
```

- `--host 0.0.0.0` (or a specific routable IP) makes the server reachable
  off-host. Loopback (`127.0.0.1`, the default) stays plain-HTTP local-only.
- `--tls-cert` / `--tls-key` are the PEM certificate chain and private key. Both
  or neither: setting one without the other is a startup error. They can also be
  supplied as `SPELUNK_SERVER_TLS_CERT` / `SPELUNK_SERVER_TLS_KEY`.
- An API key is required for any non-loopback bind (`--key` / `--key-file` /
  `SPELUNK_SERVER_KEY`). A routable bind with TLS but no key is refused, as is a
  routable bind with no TLS.

That is the whole exposure story: `https://<host>:7777` now answers, the bearer
key is required, and the connection is encrypted by the server with nothing in
front of it. The bind flags are distinct from the API-key flags on purpose:
`--tls-key` is the TLS private key, `--key`/`--key-file` is the bearer API key,
two different secrets.

## 3. Run it under systemd

Spelunk ships a first-party unit for the team server:
[`packaging/spelunk-server-team.service`](../packaging/spelunk-server-team.service).
It runs the server as a dedicated unprivileged `spelunk` user, binds a routable
interface with TLS, supplies **both** the API key and the TLS private key as
**systemd credentials** rather than environment lines, and applies standard
sandboxing. (This is the deployed team-server unit, distinct from the
per-developer local-inference user unit, `packaging/spelunk-server.service`.)

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
sudo useradd --system --home-dir /var/lib/spelunk --shell /usr/sbin/nologin spelunk
sudo install -d -o spelunk -g spelunk -m 0750 /var/lib/spelunk

# The bearer key, as a root-only 0600 file
sudo install -d -m 0755 /etc/spelunk
openssl rand -hex 32 | sudo tee /etc/spelunk/server-key >/dev/null
sudo chmod 0600 /etc/spelunk/server-key

# Bring your own TLS cert chain + private key
sudo install -m 0644 fullchain.pem /etc/spelunk/tls-cert   # public chain
sudo install -m 0600 privkey.pem   /etc/spelunk/tls-key    # root:root, private

# Install and start the unit
sudo install -m 0644 packaging/spelunk-server-team.service \
     /etc/systemd/system/spelunk-server.service
sudo systemctl daemon-reload
sudo systemctl enable --now spelunk-server
sudo systemctl status spelunk-server
```

The shipped unit's `ExecStart` is:

```ini
ExecStart=/usr/local/bin/spelunk-server \
  --host 0.0.0.0 --port 7777 \
  --db /var/lib/spelunk/spelunk.db \
  --tls-cert /etc/spelunk/tls-cert \
  --tls-key %d/tls-key
```

with both secrets loaded as credentials:

```ini
LoadCredential=server-key:/etc/spelunk/server-key
LoadCredential=tls-key:/etc/spelunk/tls-key
```

The full unit (dedicated user, hardening directives, and all) is in the repo at
[`packaging/spelunk-server-team.service`](../packaging/spelunk-server-team.service);
install it verbatim as above.

> `MemoryDenyWriteExecute=true` is a further hardening step, but the native
> embedder mmaps model weights and some backends (CUDA/BLAS) may need
> writable-executable pages: validate it against your embedder backend before
> enabling. The shipped unit leaves it commented.

**Key sources.** The credential file is preferred under systemd, but it is not
the only supported source: `SPELUNK_SERVER_KEY` remains a fully-supported equal
alternative (e.g. an `EnvironmentFile=` pointing at a `0600` root-owned file, or
set by tooling that runs the binary outside systemd). Resolution precedence is
`--key` → `--key-file` → `SPELUNK_SERVER_KEY` → `$CREDENTIALS_DIRECTORY/server-key`.
To rotate the bearer key, replace `/etc/spelunk/server-key`, `sudo systemctl
restart spelunk-server`, and redistribute it to clients. To renew the
certificate, replace `/etc/spelunk/tls-cert` (and `/etc/spelunk/tls-key` if the
key changed) and restart the service.

### Alternative: `DynamicUser=`

If you'd rather not manage a static user and data dir, use the
[`DynamicUser=` variant](../packaging/spelunk-server-team-dynamicuser.service):
systemd allocates a per-boot UID and creates `/var/lib/spelunk` via
`StateDirectory=`, chowned to that UID, so there is no `useradd` and no manual
`install -d`. Install it in place of the static-user unit; the key, certificate,
and `/etc/spelunk` provisioning above still applies:

```bash
sudo install -d -m 0755 /etc/spelunk
openssl rand -hex 32 | sudo tee /etc/spelunk/server-key >/dev/null
sudo chmod 0600 /etc/spelunk/server-key
sudo install -m 0644 fullchain.pem /etc/spelunk/tls-cert
sudo install -m 0600 privkey.pem   /etc/spelunk/tls-key
sudo install -m 0644 packaging/spelunk-server-team-dynamicuser.service \
     /etc/systemd/system/spelunk-server.service
sudo systemctl daemon-reload
sudo systemctl enable --now spelunk-server
```

The trade-off: the data dir's owner changes across boots, so any out-of-band
backup tooling must not assume a fixed UID. Prefer the static-user default when
you want a stable owner for backup/inspection or a stable `started_by` UID.

## 4. Point a remote agent at it

On the remote host (or in its container), the configuration is identical to
[R1](remote-agents.md); only the URL and the mandatory key change:

```bash
export SPELUNK_SERVER_URL=https://spelunk.example.com
export SPELUNK_SERVER_KEY=your-shared-api-key

spelunk check                 # should report the server reachable over TLS
spelunk search "auth tokens"
```

The `https://` URL points straight at the server's own TLS listener. The agent's
network path to `spelunk.example.com` is yours to provide: a VPN, Tailscale, or a
public DNS record. Spelunk does not tunnel traffic; it just needs the URL to
resolve and the server to answer.

### Trusting the server's certificate on the client

When the server's certificate chains to a public CA, agents need no extra
configuration. When it is signed by a self-signed or internal CA, point the CLI
at the CA bundle explicitly with the `SPELUNK_SERVER_CA` environment variable:

```bash
export SPELUNK_SERVER_CA=/etc/spelunk/internal-ca.pem   # PEM CA bundle
```

or set it per project in `.spelunk/config.toml`:

```toml
server_ca = "/etc/spelunk/internal-ca.pem"
```

`SPELUNK_SERVER_CA` overrides the config value. The bundle is added as a trust
anchor on top of the built-in roots. TLS verification stays on; there is no
option to disable it.

The bundle must contain the issuing **CA** certificate, not the server's leaf. A
certificate made with a plain `openssl req -x509` is a self-signed CA certificate
(basicConstraints `CA:TRUE`), and rustls rejects it if the server presents it as
its own end-entity certificate, even when you trust it here. Generate an internal
CA, issue the server a leaf certificate from it (the leaf carrying `CA:FALSE`, a
`serverAuth` extended key usage, and a SAN matching the server host), and
distribute the CA certificate as the bundle above.

## Related

- [Remote agents](remote-agents.md) – the R1 local-Docker path
- [Server setup](server.md) – Docker, keys, client config, API reference
- [Server API](architecture/server-api.md) – the HTTP + SSE surface the server exposes
