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
| **Local Docker (R1)** | A container on your machine | `http://host.docker.internal:7777` |
| Cloud-managed (R2) | A cloud workspace (e.g. Background Agents) | `https://api.spelunk.cloud` |
| Self-hosted remote (R3) | Your own VM / pod | `https://spelunk.your-domain` — see [Self-hosting](self-hosting.md) |

This page covers **R1 (local Docker)**. R2 (cloud-managed) is on the roadmap and
documented separately when it ships. R3 (self-hosted over the network) is
[Self-hosting](self-hosting.md).

## R1 — an agent in a local Docker container

The whole story is three things: an env var pointing the container's CLI at the
host's server, a bind-mount of the repo, and a bind-mount of your spelunk config
so the container knows which project it's talking to.

The local `spelunk-server` runs on the **host** (it is autostarted for you on
first use — see [Getting started](getting-started.md)). The container reaches it
across the Docker network.

### macOS / Windows (Docker Desktop)

Docker Desktop exposes the host on the special DNS name
`host.docker.internal`:

```bash
docker run --rm -it \
  -e SPELUNK_SERVER_URL=http://host.docker.internal:7777 \
  -v "$PWD":/work \
  -v "$HOME/.config/spelunk":/root/.config/spelunk \
  -w /work \
  your-agent-image
```

- `SPELUNK_SERVER_URL` points the in-container CLI at the server running on your
  host's loopback `:7777`.
- `-v "$PWD":/work` bind-mounts the repository so file paths recorded in memory
  entries mean the same thing inside the container and on the host.
- `-v "$HOME/.config/spelunk":/root/.config/spelunk` bind-mounts your spelunk
  config so the container CLI resolves the same project. (Adjust the in-container
  path if your agent image runs as a non-root user — match its `$HOME`.)
- `-w /work` runs the agent in the mounted repo.

If your host server requires authentication, also pass the key:

```bash
  -e SPELUNK_SERVER_KEY=your-shared-api-key \
```

Inside the container, the CLI now behaves exactly as it would on the host:

```bash
spelunk check                 # should report the server is reachable
spelunk search "auth tokens"  # semantic search via the host server
```

### Linux (default Docker bridge)

Plain Linux Docker (not Docker Desktop) usually has no `host.docker.internal`.
The host is reachable on the default bridge gateway, normally `172.17.0.1`:

```bash
docker run --rm -it \
  -e SPELUNK_SERVER_URL=http://172.17.0.1:7777 \
  -v "$PWD":/work \
  -v "$HOME/.config/spelunk":/root/.config/spelunk \
  -w /work \
  your-agent-image
```

If your daemon uses a custom bridge subnet, find the gateway with:

```bash
docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}'
```

<!-- TODO: confirm with Implementer — newer Docker Engine supports
     `--add-host=host.docker.internal:host-gateway` on Linux, which lets the
     macOS recipe work unchanged. Confirm the minimum Docker Engine version we
     want to recommend before promoting that form over the 172.17.0.1 fallback. -->

### Notes

- **Bind the server only to loopback.** The host server listens on
  `127.0.0.1:7777`; the Docker bridge can reach it without exposing it to your
  LAN. Do not bind the OSS server to a public interface in plain HTTP — for
  remote access over a network, terminate TLS in front of it (see
  [Self-hosting](self-hosting.md)).
- **Project identity.** Bind-mounting `~/.config/spelunk/` is the simplest way
  to share project identity. Alternatively set `SPELUNK_PROJECT_ID` explicitly
  in the container's environment.

<!-- TODO: confirm with Implementer — confirm whether `spelunk check` already
     suggests `host.docker.internal` in its unreachable-server error message
     (scope doc §3.2 calls for this hint). -->
