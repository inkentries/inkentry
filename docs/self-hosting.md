# Self-hosting spelunk-server (R3)

This is the **recommended way to run a shared team `spelunk-server`**: the binary
bare-metal on a host under systemd, bound to loopback, with an operator-owned TLS
terminator on the same host in front of it. It is the one mechanically-correct
shape for a team-reachable instance — Docker cannot host it (a container's
loopback is unreachable from the host or sibling containers; see
[Server setup → Docker: local scaffold only](server.md#docker-local-scaffold-only)).

If you operate your own `spelunk-server` and want remote agents — on a VM, a k8s
pod, or a teammate's laptop across a VPN — to reach it, you are in the
**self-hosted remote (R3)** shape. R3 is just [R1](remote-agents.md) over a
network: the same CLI, the same env vars, the same API. The only thing the
network adds is that **TLS becomes mandatory.**

`spelunk-server` terminates plain HTTP. Do **not** expose it directly on a
public or shared-network interface. Put a TLS-terminating reverse proxy in front
of it and keep the server itself bound to loopback.

> **Trust model.** Everything on the other side of that proxy — every holder of
> the shared `SPELUNK_SERVER_KEY` — is a full administrator of every project on
> this server instance; there is no per-project access control. See
> [Trust model](server.md#trust-model) and
> [ADR-056](adr/056-oss-server-tenancy-model.md) before sharing a key with more
> than one mutually-trusting group. If you need isolation between teams, run
> separate server instances (separate keys), not one shared instance.

> Spelunk does not run your agents. This page is about exposing the *memory +
> retrieval* server so remote agents can use it as a peer. It is not an agent
> runtime.

## 1. Run spelunk-server on loopback

Keep the server bound to `127.0.0.1` so the only way in is through the proxy:

```bash
spelunk-server --port 7777 --host 127.0.0.1
```

Always start the server with an API key so the exposed endpoint requires auth:

```bash
SPELUNK_SERVER_KEY=$(openssl rand -hex 32) spelunk-server --port 7777 --host 127.0.0.1
```

## 2. Terminate TLS with a reverse proxy (operator-owned)

The proxy configs below are **operator-owned reference examples**, not shipped
configuration — spelunk ships no `Caddyfile` or `nginx.conf`. The TLS terminator
is yours to own and keep current; pick whichever proxy you already run. Both
examples terminate TLS and forward to the loopback server on the same host.

### 2a. Caddy (automatic TLS)

Caddy fetches and renews a Let's Encrypt certificate automatically. A whole
`Caddyfile` for this is two lines:

```caddyfile
spelunk.example.com {
    reverse_proxy 127.0.0.1:7777
}
```

```bash
caddy run --config /etc/caddy/Caddyfile
```

That's it — `https://spelunk.example.com` now terminates TLS and forwards to the
loopback server.

### 2b. nginx

If you already run nginx, terminate TLS there (certificate via certbot or your
own CA) and proxy to loopback:

```nginx
server {
    listen 443 ssl;
    server_name spelunk.example.com;

    ssl_certificate     /etc/letsencrypt/live/spelunk.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/spelunk.example.com/privkey.pem;

    location / {
        proxy_pass         http://127.0.0.1:7777;
        proxy_http_version 1.1;
        proxy_set_header   Host $host;
        proxy_set_header   X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header   X-Forwarded-Proto $scheme;

        # The memory stream is server-sent events — don't buffer it.
        proxy_set_header   Connection '';
        proxy_buffering    off;
        proxy_read_timeout 1h;
    }
}
```

The `proxy_buffering off` / long read-timeout block matters: the `spelunk memory`
stream endpoint uses [server-sent events](architecture/server-api.md), and
default nginx buffering would stall it.

## 3. Run it under systemd

Spelunk ships a first-party unit for the team server:
[`packaging/spelunk-server-team.service`](../packaging/spelunk-server-team.service).
It runs the server as a dedicated unprivileged `spelunk` user, supplies the API
key as a **systemd credential** rather than an environment line, and applies
standard sandboxing. (This is the deployed team-server unit — distinct from the
per-developer local-inference user unit, `packaging/spelunk-server.service`.)

### Provision the key and data dir

The key is supplied through systemd's `LoadCredential=`, which reads a
`root:root 0600` file and exposes it to the process at
`$CREDENTIALS_DIRECTORY/server-key` — the binary reads it from there directly.
This keeps the key out of `systemctl show` / `/proc/<pid>/environ`, where an
`Environment=SPELUNK_SERVER_KEY=…` line would leak it to any local user.

```bash
# Dedicated user + data dir
sudo useradd --system --home-dir /var/lib/spelunk --shell /usr/sbin/nologin spelunk
sudo install -d -o spelunk -g spelunk -m 0750 /var/lib/spelunk

# The key, as a root-only 0600 file
sudo install -d -m 0755 /etc/spelunk
openssl rand -hex 32 | sudo tee /etc/spelunk/server-key >/dev/null
sudo chmod 0600 /etc/spelunk/server-key

# Install and start the unit
sudo install -m 0644 packaging/spelunk-server-team.service \
     /etc/systemd/system/spelunk-server.service
sudo systemctl daemon-reload
sudo systemctl enable --now spelunk-server
sudo systemctl status spelunk-server
```

The shipped unit:

```ini
# /etc/systemd/system/spelunk-server.service
[Unit]
Description=spelunk-server (team memory)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=spelunk
Group=spelunk

# Key as a systemd credential, exposed at $CREDENTIALS_DIRECTORY/server-key
# and read there by the binary — not a world-readable Environment= line.
LoadCredential=server-key:/etc/spelunk/server-key
ExecStart=/usr/local/bin/spelunk-server \
  --host 127.0.0.1 --port 7777 \
  --db /var/lib/spelunk/spelunk.db
Restart=on-failure
RestartSec=5

# Hardening — the server needs only its data dir and loopback.
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/spelunk
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=true
RestrictRealtime=true
RestrictSUIDSGID=true
LockPersonality=true
SystemCallArchitectures=native

[Install]
WantedBy=multi-user.target
```

> `MemoryDenyWriteExecute=true` is a further hardening step, but the native
> embedder mmaps model weights and some backends (CUDA/BLAS) may need
> writable-executable pages — validate it against your embedder backend before
> enabling. The shipped unit leaves it commented.

**Key sources.** The credential file is preferred under systemd, but it is not
the only supported source — `SPELUNK_SERVER_KEY` remains a fully-supported
equal alternative (e.g. an `EnvironmentFile=` pointing at a `0600` root-owned
file, or set by tooling that runs the binary outside systemd). Resolution
precedence is `--key` → `--key-file` → `SPELUNK_SERVER_KEY` →
`$CREDENTIALS_DIRECTORY/server-key`. To rotate the key, replace
`/etc/spelunk/server-key`, `sudo systemctl restart spelunk-server`, and
redistribute it to clients.

### Alternative: `DynamicUser=`

If you'd rather not manage a static user and data dir, use the
[`DynamicUser=` variant](../packaging/spelunk-server-team-dynamicuser.service):
systemd allocates a per-boot UID and creates `/var/lib/spelunk` via
`StateDirectory=`, chowned to that UID — no `useradd`, no manual `install -d`.
Install it in place of the static-user unit (the key/`/etc/spelunk` provisioning
above still applies):

```bash
sudo install -d -m 0755 /etc/spelunk
openssl rand -hex 32 | sudo tee /etc/spelunk/server-key >/dev/null
sudo chmod 0600 /etc/spelunk/server-key
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
[R1](remote-agents.md) — only the URL and the mandatory key change:

```bash
export SPELUNK_SERVER_URL=https://spelunk.example.com
export SPELUNK_SERVER_KEY=your-shared-api-key

spelunk check                 # should report the server reachable over TLS
spelunk search "auth tokens"
```

The agent's network path to `spelunk.example.com` is yours to provide — a VPN,
Tailscale, or a public DNS record. Spelunk does not tunnel traffic; it just
needs the URL to resolve and the TLS proxy to answer.

## Related

- [Remote agents](remote-agents.md) — the R1 local-Docker path
- [Server setup](server.md) — Docker, keys, client config, API reference
- [Server API](architecture/server-api.md) — the HTTP + SSE surface behind the proxy
