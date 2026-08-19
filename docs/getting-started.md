# Getting Started

`inkentry` is a single binary that helps you understand an unfamiliar codebase
fast: trace how a symbol connects across files, find the code behind a concept,
and assemble the context around a change, all from the CLI with no infrastructure
to stand up. Install it, run `inkentry init` inside a git repository, and the first
`search` and `context` already tell you how the code fits together.

That is the starting point. As you keep working, `inkentry` also remembers the
decisions behind the code, so a later session (yours or a teammate's) does not
re-derive them. A local `inkentry-server` is started for you on first use to add
search by meaning; you only think about a shared server when you want to share
that memory with a team (see
[Team setup](#team-setup-shared-memory-with-inkentry-server) at the end).

## 1. Install inkentry

### Windows

#### Install script (PowerShell) — recommended

The PowerShell install script resolves the latest release, downloads the
Windows `.zip`, and installs `inkentry.exe` and `inkentry-server.exe` to
`%LOCALAPPDATA%\Programs\inkentry\`. It also adds that directory to your user
`PATH` automatically.

Open PowerShell and run:

```powershell
irm https://get.inkentry.com/install.ps1 | iex
inkentry --version
```

Preview what it would do without writing anything:

```powershell
$env:INKENTRY_DRY_RUN=1; irm https://get.inkentry.com/install.ps1 | iex
```

It prints the release, the download URL, where the binaries would go, and
whether your user `PATH` would change. A pipe cannot carry a parameter, which
is why the preview is an environment variable here and a flag on Unix.

#### Scoop

[Scoop](https://scoop.sh) installs and updates `inkentry.exe` and
`inkentry-server.exe` from the release `.zip`, and keeps them current with
`scoop update`:

```powershell
scoop bucket add inkentry https://github.com/inkentries/scoop-inkentry
scoop install inkentry
inkentry --version
```

#### Manual `.zip` download

Download the `.zip` for your platform from the
[releases page](https://github.com/inkentries/inkentry/releases). The Windows
archive is named `inkentry-<version>-x86_64-pc-windows-msvc.zip`. Extract it and
place `inkentry.exe` and `inkentry-server.exe` anywhere on your `PATH`
(e.g. `C:\Users\<you>\bin\`).

> **winget:** deferred, available on request. Track the
> [releases page](https://github.com/inkentries/inkentry/releases) or the repo
> for updates.

---

### macOS and Linux

The recommended install paths are Homebrew (macOS/Linux), the install script,
and the Debian package (Linux). All three drop both `inkentry` and
`inkentry-server` onto your `$PATH`.

#### Install script (macOS and Linux) — recommended

Detects your OS/arch, resolves the latest release tag via the GitHub API,
downloads the matching tarball, and installs both binaries to `/usr/local/bin`
(or `~/.local/bin` when not run as root):

```bash
curl -fsSL https://get.inkentry.com/install.sh | sh
inkentry --version
```

Preview what it would do without writing anything:

```bash
curl -fsSL https://get.inkentry.com/install.sh | sh -s -- --dry-run
```

#### Homebrew (macOS and Linux)

```bash
brew install inkentries/inkentry/inkentry
brew trust inkentries/inkentry
inkentry --version
```

Homebrew 6 will not evaluate a third-party tap's formula unless the tap is
trusted or the formula is named in full, which is why the install line above is
fully qualified. `brew trust` is what keeps it working afterwards: an untrusted
tap is dropped from the list `brew upgrade` and `brew outdated` walk, so
inkentry would simply never appear as upgradable. On Homebrew 5 and earlier
`brew trust` does not exist — skip that line.

### Debian / Ubuntu (`.deb`)

The release pipeline publishes an `amd64` `.deb`. Substitute the release version
for `<version>` (e.g. `0.8.0`). The download path is pinned to the release tag
(`v<version>`) so the versioned filename always resolves — the version-free
`releases/latest/download/…` form 404s on a versioned asset name (see #340):

```bash
curl -fsSLO https://github.com/inkentries/inkentry/releases/download/v<version>/inkentry_<version>_amd64.deb
sudo apt install ./inkentry_<version>_amd64.deb
inkentry --version
```

> Install with `apt`, not `dpkg -i`. The package declares the shared libraries
> the binaries link against (including `libdbus-1-3`); `apt` pulls those in,
> whereas `dpkg -i` does not resolve dependencies and leaves the package
> unconfigured on a machine that lacks them. The leading `./` is required, or
> `apt` treats the argument as a package name instead of a file.

### Manual tarball / zip (any platform)

binaries on your `$PATH`. Supported targets:

| Platform | Archive name | Notes |
|----------|-------------|-------|
| macOS (Apple Silicon) | `inkentry-<version>-aarch64-apple-darwin.tar.gz` | |
| Linux x86_64 | `inkentry-<version>-x86_64-unknown-linux-gnu.tar.gz` | Requires glibc 2.31 (Debian 11 Bullseye or newer / Ubuntu 20.04 or newer); on minimal images, install `libdbus-1-3` |
| Linux ARM64 | `inkentry-<version>-aarch64-unknown-linux-gnu.tar.gz` | Requires glibc 2.31 (Debian 11 Bullseye or newer / Ubuntu 20.04 or newer); on minimal images, install `libdbus-1-3` |
| Windows x86_64 | `inkentry-<version>-x86_64-pc-windows-msvc.zip` | |

> **Intel Macs (`x86_64-apple-darwin`):** prebuilt binaries are not published for
> this target — Apple deprecated the architecture and Apple Silicon replaced it on
> new hardware six years ago. Intel Mac users must build from source; see
> [Building from source](building.md) (`cargo build --release` works unmodified on
> `x86_64-apple-darwin`).

```bash
# Example: macOS Apple Silicon. Replace <version> with the release tag, e.g. v0.9.0
curl -L https://github.com/inkentries/inkentry/releases/download/<version>/inkentry-<version>-aarch64-apple-darwin.tar.gz \
  | tar -xz && chmod +x inkentry inkentry-server && sudo mv inkentry inkentry-server /usr/local/bin/

# Verify
inkentry --version
```

Swap the target in the filename for another platform. Building from source? See
[Building from source](building.md).

### Running inkentry-server as a service (optional)

The release artifacts include service units for keeping a local server running:
a launchd plist (`packaging/inkentry-server.plist`) for macOS and a systemd unit
(`packaging/inkentry-server.service`) for Linux. Most users don't need these —
`inkentry` autostarts the server on demand (see section 2) — but they're useful
on a shared or always-on host.

### Upgrading from spelunk (0.9.8 or earlier)

inkentry refuses a `memory.db` written by spelunk rather than converting it, so
**export before you upgrade**. The export tool is **`spelunk-export`**, a
standalone per-platform asset on the [spelunk-cloud/spelunk
releases](https://github.com/spelunk-cloud/spelunk/releases) page — deliberately
not part of the inkentry archive, the `.deb`, the Homebrew formula or the Scoop
manifest, so upgrading through a package manager will not put it on your machine.
Run it against the old project, then bring the dump across:

```bash
inkentry import <dump>
```

If your entries were also shared through git notes, renaming the ref hydrates
them directly — but it carries **only the notes-resident entries** (on one real
corpus, 51 of 343), so it is not a substitute for the export:

```bash
git fetch <old-remote> 'refs/notes/spelunk:refs/notes/inkentry'
```

`index.db` needs no migration at all: run `inkentry init` and it is rebuilt from
your source tree.

That is the whole of it for one machine. **If you share a repository with
anyone, it is not a personal upgrade**: `.inkentry/config.toml` is tracked, and
so is every committed script, CI step and agent instruction that calls the CLI,
so one person's commit reaches colleagues still on the old binary. See
[Upgrading](upgrading.md) for the team sequence, the symptoms, and what to do if
you already upgraded without exporting.

## 2. Cold start: index and get your first answer

```bash
cd /path/to/your/project
inkentry init
```

That's the whole setup. `inkentry init` registers the project, starts the bundled
`inkentry-server` in the background when run interactively (if one isn't already running),
parses and chunks every source file, and hands the embedding pass to a detached background
worker so the prompt returns after parsing rather than after the full embed pass. Embeddings build in the background; full-text search
works immediately, and semantic search becomes available as embeddings land.

```bash
# Search by meaning, not just text
inkentry search "where do we validate auth tokens"
```

`init` also writes `.inkentry/.gitignore` so the machine-specific SQLite
(`index.db*`, `memory.db*`) and the per-run index lock (`index.lock*`, whose
`.pid` sidecar holds a local process id) stay out of version control, and records the
project slug as `project_id` in `.inkentry/config.toml`. The slug defaults to the
git-derived identity (`host/owner/repo` when an `origin` remote exists, else
`local/<blake3-hex>` of the path); pass `inkentry init --name <slug>` to set an
explicit one for a repo without a remote.

`init` writes `.inkentry/config.toml` but takes no git action on it — **commit it
yourself** so your project slug travels with the repo and the whole team
resolves to one project identity:

```bash
git add .inkentry/config.toml && git commit -m "Add inkentry project slug"
```

This is a step you own, not something `init` does for you. Without a committed
slug, a fresh clone of a remote-less repo derives a different per-clone identity,
and an explicit `--name` slug cannot be re-derived at all — either way the team
would resolve to more than one project until the file is committed. An existing
`project_id` or `.inkentry/.gitignore` is never overwritten, so re-running `init`
is safe.

Inside a git repository with an `origin` remote, `init` also writes two entries
to that clone's `.git/config`, and prints that it did:

| Entry | Value | Why |
|---|---|---|
| `remote.origin.fetch` (added, not replaced) | `+refs/notes/inkentry*:refs/notes/origin/inkentry*` | teammates' memory notes arrive on your next `git fetch` |
| `notes.rewriteRef` | `refs/notes/inkentry` | your memory notes survive `git commit --amend` and `git rebase` |

Both are additive, local to this clone, and confined to inkentry's own notes
namespace. `init` deliberately sets no push refspec and never modifies your
remote, so neither entry publishes anything: sharing your own memory is a
separate opt-in step, covered in section 5.

No config file, no Docker, no external embedder. The server bundles a native
embedding model (codefuse-ai/F2LLM-v2-330M, 896-dim, GPU-accelerated on macOS
via candle); a pre-quantized Q8_0 GGUF (~339 MB) is downloaded once on first use
and cached under `~/.local/share/inkentry/models/`. No LM Studio or other
external inference server is needed. The next section covers commands
that work even before you index.

You can manage the background server explicitly if you want:

```bash
inkentry server start     # start the local daemon (idempotent; auto-binds 127.0.0.1)
inkentry server status    # PID, port, instance id, uptime
inkentry server logs      # last 50 lines of the server log
inkentry server stop      # stop the daemon
```

In non-interactive contexts (CI, agent harnesses) `inkentry init` does **not**
auto-spawn the server — run `inkentry server start` first if you want semantic
search there, or set `INKENTRY_NO_SERVER=1` to stay fully offline.

## 3. Start using it inside your project

`search` requires an index: run `inkentry init` first — an uninitialised
directory funnels you there. Full-text results are available as soon as `init`
parses the tree; semantic ranking builds in the background. Memory and `context`
also operate on the local project you created with `inkentry init` (step 2); in
an un-initialized directory they fail closed with a `no inkentry project here`
error instead of using a machine-global store. From inside your project you can:

```bash
# Find the code behind a concept: search takes any phrase, no symbol name needed
inkentry search "error handling" --only-text

# Trace how a symbol connects: --graph appends the symbol's chunk and its 1-hop
# call-graph neighbours after the ranked results (use a real symbol name, e.g.
# one you just saw in the results above)
inkentry search validate_token --graph

# Store a decision for your team
inkentry memory add --kind decision \
  --title "Chose token bucket for rate limiting" \
  --body "Simpler than sliding window; sufficient for <1k RPS"

# List your decisions
inkentry memory list --kind decision
```

Memory is stored locally in the project's `.inkentry/memory.db` and mirrored
into git notes (`refs/notes/inkentry`) by default, so it stays with your clone
rather than in a service you have to run. Getting it to teammates is a separate
step you opt into: `inkentry hooks install --pre-push` publishes those notes on
`git push` (see section 5). Reading their memory needs nothing installed, since
`init` already configured the fetch side.

## 4. Start an agent session

When your agent or team is starting a new coding session, pull all relevant context in one command:

```bash
# Agent entry point — pulls decisions, requirements, questions, handoffs
inkentry context

# Filter by kind
inkentry context --kind decision

# Get JSON for machine processing
AGENT=true inkentry context
```

The default output is compact (a few recent entries per section). Pass
`--budget <N>` (alias `--max-tokens`) to cap total output at N tokens, or
`--limit <N>` to widen the per-section count.

## 5. Git hooks: harvesting and sharing (optional)

Two hooks are available, and they do unrelated jobs. Neither is installed for
you.

### Post-commit: harvest memory from new commits

```bash
inkentry hooks install
```

This writes a post-commit hook that brings the index up to date and extracts
memories from each new commit, both detached so `git commit` is never blocked.

**Harvesting needs an LLM, and a default install has none.** It is the only
feature in inkentry that does: indexing, full-text and semantic search, chunk
summaries and every `inkentry memory` command work with no LLM configured. So
the hook runs happily from the day you install it while harvesting nothing, and
starts producing entries once you point inkentry at an OpenAI-compatible
chat-completions endpoint. See [Using your own LLM
endpoint](#using-your-own-llm-endpoint-advanced) for that, and note the caveat
there about restarting a daemon that is already running.

### Pre-push: publish your memory to the remote

```bash
inkentry hooks install --pre-push
```

From then on, every `git push` to a named remote merges the remote's memory
notes into yours and pushes `refs/notes/inkentry` alongside your commits. A
publish that fails warns on stderr and exits 0, so sharing memory can never cost
you your push.

Reading and publishing are deliberately asymmetric, and the asymmetry is the
design rather than an oversight:

- **Reading a teammate's memory is automatic.** `init` configured the fetch
  refspec (section 2), so their notes arrive on your next `git fetch` with
  nothing to install.
- **Publishing your own is opt-in.** Until you install this hook, or push
  `refs/notes/inkentry` by hand, your memory stays in your clone. inkentry will
  not quietly change what your `git push` sends.

Tying publication to `git push` is also what keeps notes resolvable: a note on a
commit you have not pushed could otherwise reach the remote while the commit it
describes does not.

### Removing them

Developers who do not have `inkentry` installed are unaffected by either hook.
To remove both:

```bash
inkentry hooks uninstall
```

A pre-existing hook that inkentry did not write is reported and left alone,
never overwritten or deleted. For the full behaviour, including `--ci` and how
the hooks directory is resolved under husky or lefthook, see the [commands
reference](commands.md#inkentry-hooks).

---

## Capability tiers: where inference and memory live

inkentry works at three tiers; the team-memory tier can be a server you host
yourself or the managed inkentry cloud, shown as separate rows below. You do not
pick one by hand; inkentry uses the best one available and degrades cleanly when a
server is not reachable. The load-bearing distinction is that a **local server
does inference only and never stores memory**. Your memory always lives in the
project's local `memory.db` until you *explicitly* configure a team server or use
the managed inkentry cloud.

| Tier | What runs it | What it adds | Where memory lives |
|---|---|---|---|
| **Built-in** (zero infra) | just the `inkentry` binary | git-notes memory, full-text search, code graph | local `memory.db` |
| **Local semantic server** | a loopback `inkentry-server`, auto-started on demand | semantic / hybrid `search` | still local `memory.db`: the server is **inference only, never a memory store** |
| **Team memory server** | a shared `inkentry-server` you deploy, set via an explicit `server_url` | shared memory across the team | the shared server you run: memory leaves your machine, your code stays local |
| **inkentry cloud** (hosted) | a managed service: nothing to deploy or maintain | the same shared-team memory as a self-hosted server, without running one | the hosted service: memory leaves your machine, your code stays local |

Built-in works with nothing installed but the binary (the always-available
commands in section 3). The local semantic server is auto-discovered on loopback
(`127.0.0.1`) and started for you the first time a command needs it; it embeds
queries and runs LLM calls, but a project's memory stays in `memory.db`
regardless of whether it is running. Memory moves off the local machine only when
you point at a team server, self-hosted via an explicit `server_url` or the
managed inkentry cloud (see
[Team setup](#team-setup-shared-memory-with-inkentry-server)); each developer's
code still stays local.

To stay fully offline (CI, air-gapped, or you just don't want a background
process), set `INKENTRY_NO_SERVER=1`: inkentry then runs built-in only, and
inference-only commands exit with a clear message instead of starting anything.

For how discovery works and how to point the CLI at a remote server, see
**[Server setup](server-setup.md)** and
[CLI capability tiers](architecture/capability-tiers.md).

### Using your own LLM endpoint (advanced)

By default the bundled `inkentry-server` provides embeddings (native, via the
candle-served F2LLM-v2-330M model, 896-dim) and, when a chat model is
configured, LLM inference. The embedding **model and its compute path are
both fixed** product-wide: `inkentry` always embeds through the bundled native
embedder, and there is no way to relocate or swap it. LLM inference is
different: the server has no LLM of its own, so you point it at your own
OpenAI-compatible chat-completions endpoint (LM Studio, Ollama, vLLM, a
self-hosted gateway).

Set it once in your **personal** config, and every daemon the CLI starts is
configured with it:

```toml
# ~/.config/inkentry/config.toml
llm_url = "http://127.0.0.1:1234"
llm_model = "your-chat-model-id"
```

If the endpoint needs a credential, store it once:

```bash
inkentry auth set-key --llm
```

It is read from stdin or a prompt and kept in your OS secret store, never in a
config file and never in a command-line argument. The CLI resolves it when it
starts the daemon and hands it over in the child's environment; the daemon
never reads your keychain itself, because a detached background process cannot
answer the authorization prompt that would raise.

Then restart the daemon, because one that is already running keeps the
configuration it started with:

```bash
inkentry server stop     # if one is already running
inkentry server start    # starts with the endpoint configured above
```

`INKENTRY_LLM_URL`, `INKENTRY_LLM_MODEL`, and `INKENTRY_LLM_KEY` override the
config file and the stored credential, and `inkentry server start --llm-url` /
`--llm-model` override those in turn for a single daemon.

Or, if you run `inkentry-server` yourself, pass the flags directly:

```bash
inkentry-server --llm-url http://127.0.0.1:1234 --llm-model your-chat-model-id
```

Add `--llm-key-file /path/to/key` (or set `INKENTRY_LLM_KEY`) if the endpoint is
keyed. With a credential configured, a plaintext `http://` endpoint on anything
but loopback is refused at startup rather than sending the credential in the
clear: use `https://` for a remote endpoint. A keyless endpoint is unaffected,
so an existing LM Studio or Ollama box on your LAN keeps working.

`harvest` and index-time chunk summaries both pick up an
LLM-configured local daemon automatically, and fall back to a `server_url` that
provides an LLM when your local one does not.

Two things are worth knowing before you hit them:

- **A daemon that was already running does not have your new `llm_url`.** In
  that case inkentry stops and asks you to restart it rather than falling back to
  a remote LLM, so under the default `local_first` mode a configured local
  endpoint means your code is not sent elsewhere. That guarantee does not hold
  under `mode = "cloud_first"`, where `server_url` is the inference target
  already.
- **`inkentry index` never reaches for an LLM at all.** Chunk summaries are
  composed deterministically from the parse, with no model, no key and no
  network, so there is nothing here for a missing LLM to affect. Pass
  `--no-summaries` to skip that pass. `harvest` does fail without an LLM, since
  it cannot run without one.

See [Third-party models](third-party-models.md#how-inkentry-finds-an-llm) for the
routing rule, the exact messages, the full precedence and security details, and
the team-server equivalent.

This is an advanced override; most users never set it: `harvest` is the only
thing unavailable without an LLM configured, and both semantic search and chunk
summaries work regardless — the native embedder needs no configuration at all,
and the summaries need no inference at all.

### Index your project for semantic search

`inkentry init` (section 2) already indexes and embeds your project against the
local server. If you've configured a custom embedding endpoint above, restart
the server and run `init` again so chunks are embedded through that endpoint:

```bash
cd /path/to/your/project
inkentry init
```

This:
1. Registers your project in the global registry
2. Parses every source file and indexes chunks
3. Embeds chunks using your configured server
4. Stores everything in `.inkentry/index.db` inside the project

Output:
```
inkentry initialised for github.com/acme/my-project

  Index:   142 files, 1 840 chunks
  DB:      /path/to/your/project/.inkentry/index.db
  Project: github.com/acme/my-project  (written to .inkentry/config.toml)
  Embeddings: 1 840 vectors
```

**Subsequent runs** only re-index changed files (via blake3 hash), and also
backfill embeddings for any chunk that was parsed but never embedded (for
instance if the embedder model was still loading on the first run):

```bash
inkentry index /path/to/your/project
```

Force a full re-index after changing the embedding model:

```bash
inkentry index /path/to/your/project --force
```

### Use semantic search

```bash
# Finds code by concept, not just text (unified over code + memory, best available)
inkentry search "error handling in the HTTP layer"

# Code corpus only (skip interleaved memory results)
inkentry search "authentication" --only-code

# Append the symbol's 1-hop call-graph neighbours after the ranked results
inkentry search "authentication" --graph

# Fit results within a token budget for agents
inkentry search "database layer" --budget 4000

# Machine-readable output
inkentry search "database migrations" --format json
```

### Check index health

```bash
inkentry status                              # index statistics
inkentry index .                             # bring the index up to date (idempotent, blake3-gated)
inkentry plumbing ls-files --stale           # list files that need re-indexing (JSONL; no rows = fresh)
```

`ls-files` is a plumbing command, so it uses grep-style exit codes: printing no
rows exits **1**, meaning a fully fresh index reads as a non-zero exit. Fine
interactively, worth guarding in a `set -e` script. `inkentry status` is the
porcelain health check.

---

## Next steps

- [Commands reference](commands.md) — every flag and option
- [Memory](memory.md) — storing project context across sessions
- [Agent Guide](agent-guide.md) — using `inkentry` with AI coding agents
- [Remote agents](remote-agents.md) — running an agent in a Docker container against your local server
- [Server setup](server-setup.md): exposing inkentry-server to remote agents over TLS
- [Building from source](building.md) — for contributors and platform builders

---

## Team setup: Shared memory with inkentry-server

Working with a team? Point everyone at a shared `inkentry-server` so they share decisions, requirements, and context instead of siloing them locally. This is a *different* server from the local one inkentry autostarts for inference — it's a long-lived, deployed instance with an API key.

Each team member's code stays local — only memory travels to the server.

### Quick setup

Add `.inkentry/config.toml` at your repo root (commit it):

```toml
# .inkentry/config.toml — commit this, no secrets
server_url = "https://inkentry.internal.example.com"
project_id = "my-awesome-app"
```

> **`server_url` must be `https://` unless it points at loopback**
> (`127.0.0.1` / `::1` / `localhost`) — a non-loopback `http://` URL is
> rejected at startup, with no opt-out, because the CLI attaches your bearer
> token to these requests. See [Server setup](server-setup.md) for putting TLS
> in front of a deployed server.

Each developer provides their own key with `inkentry auth set-key`, scoped to
this server's URL:

```bash
inkentry auth set-key --server https://inkentry.internal.example.com
```

The key is stored in your OS keychain (macOS Keychain, Linux Secret Service,
Windows Credential Manager) rather than in plaintext, keyed by the server's
origin so a second project's server key never collides with this one. For CI /
headless use, the `INKENTRY_SERVER_KEY` environment variable works everywhere
and takes precedence over the stored key:

```bash
export INKENTRY_SERVER_KEY="your-shared-api-key"
```

A `server_key` line in a config file is not read, in either the personal
`~/.config/inkentry/config.toml` or a project's checked-in
`.inkentry/config.toml`. It is not migrated into the per-server store either:
inkentry names the file on stderr and tells you to rotate the key it holds,
because a credential that sat in a plaintext file should be treated as exposed.
Run the `set-key` command above with the replacement, and delete the line
yourself. A key belongs to a developer, not to a committed file. On a host with
no keychain, inkentry falls back to an owner-only
`~/.config/inkentry/secrets.toml`. For the full
credential-storage rules and the `INKENTRY_SECRET_STORE` override, see the
[Commands reference](commands.md#inkentry-auth).

`project_id` stays a human-readable slug, and it is sent to the server exactly
as configured. Both a self-hosted inkentry-server and the hosted cloud API accept
either a slug or a UUID as the project key, so nothing is looked up and nothing
is cached. See [Server setup](server-setup.md#client-configuration) for details.

After setup, all `inkentry memory` commands transparently use the server. Seed it
with your existing local memory, then keep recording decisions as usual:

```bash
inkentry sync  # two-way: push your existing local entries up, pull teammates' entries down
```

`sync` is safe to re-run: with nothing new to push it says so and exits 0.

> There is a one-way equivalent, `inkentry plumbing push`, but reach for it only
> in a script you have written the exit handling for. Plumbing commands use
> grep-style exit codes, and `push` exits **1** whenever no entry was newly
> created. That includes the entirely successful case where the server already
> has everything, so under `set -e` or in a CI job a clean run reads as a
> failure. `sync` is the porcelain command and exits 0 there.

Beyond that first seed, in the default `local_first` mode you rarely run
`inkentry sync` by hand. Your writes commit to the local `memory.db` immediately
and never block on the network; from an interactive terminal a background
reconciler then drains what you recorded up to the server and pulls teammates'
entries down, so the shared memory converges on its own. `inkentry sync` is the
explicit escape hatch for when you want that reconcile to happen synchronously
now rather than in the background, such as a CI job that needs entries pushed
before it exits. Code never travels; only memory does.

For full setup and deployment guide: **[Server setup](server-setup.md)**: Docker, configuration, API reference.

### Enterprise / MDM deployment

Rolling inkentry out to a managed fleet? The
[`examples/mdm/`](examples/mdm/README.md) directory shows how to deploy and
pre-configure `inkentry` and `inkentry-server` via MDM (managed config file,
fleet-wide environment, and a macOS profile for a managed server daemon),
grounded in inkentry's real config surface.
