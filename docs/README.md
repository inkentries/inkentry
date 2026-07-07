# spelunk documentation

> git tracks what changed. spelunk remembers why.

spelunk helps you understand an unfamiliar codebase fast, then remembers the
decisions behind it so the next session does not re-derive them. These docs
follow the path a new user takes, from the first five minutes to running a
shared memory server for a team. Read them in order the first time; use the
reference (stage 4) for lookup afterwards.

## 1. On-ramp (first five minutes)

Understand how an unfamiliar codebase fits together, with zero infrastructure.
Install the binary, run `spelunk init`, and the first `graph` / `search` /
`context` already trace how a symbol connects, find the code behind a concept,
and assemble the context around a change. This is fast understanding (how,
where, what), not a faster grep, and it needs no server.

- [README quick start](../README.md#quick-start): the install one-liner and three commands that work immediately
- [Getting Started → install](getting-started.md#1-install-spelunk): script, Homebrew, `.deb`, or tarball
- [Getting Started → first index and retrieval](getting-started.md#2-cold-start-index-and-get-your-first-answer): `init`, `index`, and your first answer
- [Example: onboarding a new codebase](examples/onboarding-a-new-codebase.md): a full first-session walkthrough

## 2. Getting started (the happy path)

Make that understanding stick. Run the core loop end to end on built-in storage
(git-notes memory, full-text and code graph, no daemon), record your first
decision by hand, and watch a later `spelunk context` hand it back. Then take one
step up to the local semantic server for search by meaning. That local server is
an inference backend only: it embeds queries and runs summaries, and it never
stores memory. Your memory always lives in the project's local `memory.db`.

- [Getting Started](getting-started.md): the core loop and the local semantic tier
- [Memory](memory.md): decisions, requirements, and context; supersede, do not delete

## 3. Configure your agent

Wire an AI coding agent to spelunk so it pulls context before it edits and
records decisions as it works. The payoff is that the why-layer fills itself:
install the git hook and every commit runs `spelunk memory harvest`, so the
reasoning behind the code is captured automatically, with no separate step to
sit down and write docs. This is what completes the how-to-why arc: the decisions
stage 1 could not yet show now accumulate on their own.

- [Agent Guide](agent-guide.md): the session pattern, automatic capture, and machine-readable output
- [AGENT.md template](examples/AGENT.md): drop-in instructions telling an agent to use spelunk
- [Claude Code skill](../SKILL.md): spelunk packaged as an agent skill
- Automatic capture: [`spelunk hooks install`](commands.md#spelunk-hooks) plus [`spelunk memory harvest`](commands.md#spelunk-memory)

## 4. Reference

Look up exact behaviour once you have the mental model. Every shipped command is
documented and verified against the binary; reference lives after the journey, not
before it.

- [Commands](commands.md): every subcommand, flag, and environment variable
- [Memory model](memory.md): kinds, cross-project visibility, git-notes write-through
- [Architecture](architecture.md) and [capability tiers](architecture/capability-tiers.md)
- [Plumbing and porcelain](plumbing-and-porcelain.md): JSONL commands for scripts and agents
- [Security](security/THREAT-MODEL.md): threat model and boundaries (secret scanning is defense-in-depth, not a boundary)

## 5. Local vs server vs team-server

The three tiers, stated once. A local server does inference only; an explicit
team `server_url` is the only thing that moves memory off your machine.

| Tier | What it adds | Where memory lives |
|---|---|---|
| Built-in (zero infra) | git-notes memory, full-text and ast-grep search, code graph | local `memory.db` |
| Local semantic server (auto-started on loopback) | semantic search, `explore`, summaries | still local `memory.db`: inference only, never a memory store |
| Team memory server (explicit `server_url`) | shared memory across a team | the shared server: the only path off the local machine |

Point a team at a self-hosted spelunk server with an explicit `server_url` and a
shared server key, then use [`spelunk sync`](commands.md#spelunk-sync) to push
your decisions up and pull teammates' decisions down. Each developer's code stays
local.

- [Getting Started → capability tiers](getting-started.md#capability-tiers-where-inference-and-memory-live): the tier table in context
- [Getting Started → team setup](getting-started.md#team-setup-shared-memory-with-spelunk-server): how to set `server_url` and sync
- [Server setup](server.md) and [Self-hosting](self-hosting.md): deploy and expose a team server
- [Remote agents](remote-agents.md): run an agent in a container against your server

Running spelunk against the hosted spelunk.cloud service is the next step for
teams that would rather not operate a server; it is covered in the cloud
documentation, not here.

---

Contributing? See [Building from source](building.md).
