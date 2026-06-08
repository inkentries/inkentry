# Self-hosting spelunk-server (R3)

If you operate your own `spelunk-server` and want remote agents — on a VM, a k8s
pod, or a teammate's laptop across a VPN — to reach it, you are in the
**self-hosted remote (R3)** shape. R3 is just [R1](remote-agents.md) over a
network: the same CLI, the same env vars, the same API. The only thing the
network adds is that **TLS becomes mandatory.**

`spelunk-server` terminates plain HTTP. Do **not** expose it directly on a
public or shared-network interface. Put a TLS-terminating reverse proxy in front
of it and keep the server itself bound to loopback.

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

## 2a. Reverse proxy — Caddy (automatic TLS)

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

## 2b. Reverse proxy — nginx

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

A unit to keep the server running on boot, bound to loopback:

```ini
# /etc/systemd/system/spelunk-server.service
[Unit]
Description=spelunk-server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=spelunk
Environment=SPELUNK_SERVER_KEY=your-shared-api-key
ExecStart=/usr/local/bin/spelunk-server --port 7777 --host 127.0.0.1 --db /var/lib/spelunk/spelunk.db
Restart=on-failure
RestartSec=5

# Hardening — the server only needs its data dir and loopback.
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/spelunk

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now spelunk-server
sudo systemctl status spelunk-server
```

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
