# Remote agents

A *remote agent* is an AI coding agent process that does not share a filesystem
or local network with the workstation that owns your code. Spelunk supports
these agents the same way it supports a local one: the agent installs the
`spelunk` CLI, points it at a `spelunk-server`, and gets the same memory +
retrieval surface a local agent gets.

Spelunk **does not run agents.** The server is to an agent what an LSP server is
to an editor — a long-running peer it talks to, not a runtime that hosts it.
There is no relay, no tunnel, and no agent supervision. Everything below is
configuration and defaults, not new behaviour.

The shapes we distinguish:

| Shape | Where the agent runs | `SPELUNK_SERVER_URL` |
|---|---|---|
| Local (R0) | Your workstation | `http://127.0.0.1:7777` (auto) |
| **Local Docker (R1)** | A container on your machine | `https://spelunk.your-domain` (portable) — or `http://host.docker.internal:7777` on Docker Desktop only |
| Cloud-managed (R2) | A cloud workspace (e.g. Background Agents) | `https://api.spelunk.cloud` |
| Self-hosted remote (R3) | Your own VM / pod | `https://spelunk.your-domain` — see [Self-hosting](self-hosting.md) |

This page covers **R1 (local Docker)**. R2 (cloud-managed) is on the roadmap and
documented separately when it ships. R3 (self-hosted over the network) is
[Self-hosting](self-hosting.md).

## R1 — an agent in a local Docker container

A containerized agent needs three things: an env var pointing its CLI at a
`spelunk-server`, a bind-mount of the repo, and a bind-mount of your spelunk
config so it resolves the same project.

The one detail that trips people up is **which URL** the container uses. A local
`spelunk-server` binds the host's loopback (`127.0.0.1`), and a container's
network namespace cannot reach the host's loopback by any portable means — so
the reliable answer is to point the container at the team server's **HTTPS
endpoint**, the same `https://` URL any other client uses, not at a Docker bridge
address.

### Recommended: point at the server's HTTPS endpoint (portable)

Stand up the team server the [Self-hosting](self-hosting.md) way (a routable
bind with `--tls-cert`/`--tls-key` and a key, where the server terminates HTTPS
itself) and point the container at its `https://` hostname. This works
identically on Docker Desktop and native Linux, because it's a routable HTTPS
URL, not a host-loopback address:

```bash
docker run --rm -it \
  -e SPELUNK_SERVER_URL=https://spelunk.example.com \
  -e SPELUNK_SERVER_KEY=your-shared-api-key \
  -v "$PWD":/work \
  -v "$HOME/.config/spelunk":/root/.config/spelunk \
  -w /work \
  your-agent-image
```

- `SPELUNK_SERVER_URL` points the in-container CLI at the team server's own
  HTTPS endpoint, which the server serves directly.
- `SPELUNK_SERVER_KEY` is the shared API key (required — a networked server is
  always keyed; see [Self-hosting](self-hosting.md)).
- `-v "$PWD":/work` bind-mounts the repository so file paths recorded in memory
  entries mean the same thing inside the container and on the host.
- `-v "$HOME/.config/spelunk":/root/.config/spelunk` bind-mounts your spelunk
  config so the container CLI resolves the same project. (Adjust the in-container
  path if your agent image runs as a non-root user — match its `$HOME`.)
- `-w /work` runs the agent in the mounted repo.

Inside the container, the CLI behaves exactly as it would on the host:

```bash
spelunk check                 # should report the server reachable over TLS
spelunk search "auth tokens"  # semantic search via the server
```

The **server side** of this (the routable TLS bind and the systemd unit) is the
bare-metal path in [Self-hosting](self-hosting.md).

### Docker Desktop convenience (solo, non-portable)

If you're a solo developer with only the auto-started local loopback server (no
team server) and you're on **Docker Desktop** (macOS/Windows), Docker Desktop
special-cases the DNS name `host.docker.internal` to reach the host's loopback,
so you can skip the TLS endpoint and point straight at the host server:

```bash
docker run --rm -it \
  -e SPELUNK_SERVER_URL=http://host.docker.internal:7777 \
  -v "$PWD":/work \
  -v "$HOME/.config/spelunk":/root/.config/spelunk \
  -w /work \
  your-agent-image
```

This is a **Docker Desktop-only** convenience. It does not work on native Linux
Docker (see below), so don't bake it into anything you also run on Linux.

### Native Linux: no loopback shortcut

Plain Linux Docker (not Docker Desktop) has **no** way to reach a
host-loopback-bound server from a container:

- The default bridge gateway (`172.17.0.1`) and
  `--add-host=host.docker.internal:host-gateway` both resolve to the host's
  **routable** interface, not its loopback, so a local server bound to
  `127.0.0.1` is not listening where the container can reach it.
- Binding the server to the bridge address over plaintext instead is a
  **non-loopback plaintext bind, which `spelunk-server` refuses
  unconditionally**: there is no override. A routable bind is only allowed with
  `--tls-cert`/`--tls-key` and a key, i.e. the HTTPS endpoint below.

So on native Linux there is no bridge shortcut: use the recommended HTTPS-endpoint
path above. The team server's own routable TLS listener (per
[Self-hosting](self-hosting.md)) is what makes it reachable from a container, and
it does so the same way on every platform.

### Notes

- **Project identity.** Bind-mounting `~/.config/spelunk/` is the simplest way
  to share project identity. Alternatively set `SPELUNK_PROJECT_ID` explicitly
  in the container's environment.
