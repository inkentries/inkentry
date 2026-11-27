# What inkentry is

This file describes the product as it stands on `main`, in domain language. It covers
what the product is and does. It leaves out how the product is built, why, how it got
here, and what comes next. It is the reference for agentic PR review and is kept
current by a weekly review of merged pull requests and ADRs.

Statement IDs (`MEM-3`) are stable. A changed statement keeps its ID. A removed
statement's ID is not reused.

## Identity

- **ID-1** git tracks what changed. inkentry remembers why.
- **ID-2** inkentry is your project's memory. It keeps the decisions that shape a
  codebase next to the code they shaped, and hands them back when they matter.
- **ID-3** Built for people, with the agents working alongside them as first-class users.
- **ID-4** The product is three actions: **context**, **search**, **memory**.
- **ID-5** Open source under the MIT licence: the CLI, the agent skill and the
  self-hosted team server.

## Actors

- **ACT-1** *Developer*: sets up a project, searches it, records and reads decisions.
- **ACT-2** *Agent*: an AI coding agent that starts each session with context,
  searches before it edits, and records decisions and handoffs as it works.
- **ACT-3** *Team*: developers and agents sharing one project's memory.
- **ACT-4** *Operator*: runs a self-hosted team server for a team.

## Domain

| Term | Meaning |
|---|---|
| Project | A repository set up with inkentry. It has one identity that the whole team shares. |
| Memory entry | A kept piece of project knowledge: a kind, a title, a body, and optionally tags and linked files. |
| Kind | One of: decision, requirement, context, note, question, answer, handoff, intent, antipattern. |
| Intent | An entry signalling active work. It drives warnings when work overlaps. |
| Handoff | The state passed from one session or agent to the next. |
| Handle | A short reference to an entry that is the same on every machine holding it. |
| Supersede | Replace an entry with a newer one. The old entry is archived, never deleted. |
| Relates to / contradicts | Links between entries. A contradiction is surfaced, never resolved automatically. |
| Anchor | The commit an entry belongs to: the commit that carried the work it describes. |
| Linked project | Another local project whose locked or cross-project decisions and requirements appear here. |
| Harvest | Drawing decisions out of commit history or agent session logs. |
| Team server | A self-hosted server a team shares memory through. |
| inkentry cloud | The hosted alternative to a team server, opted into per project. |

## Context

- **CTX-1** One command gives a session brief: decisions, requirements, open
  questions, handoffs, active intents, and warnings when work overlaps.
- **CTX-2** The brief can be limited to a size budget, a kind, a tag or a file.
- **CTX-3** The brief includes conventions inferred from the code, with no model or server.

## Search

- **SRCH-1** One search covers code and memory and returns one ranked list. Each
  result is marked as code or memory.
- **SRCH-2** Memory appears in a search only when it is relevant to the query.
- **SRCH-3** Search can be narrowed to code, to memory, or to keyword-only. It can also
  show a symbol's callers and callees.
- **SRCH-4** Search requires a set-up project. Keyword search works as soon as setup
  finishes. Concept search fills in while the project is being indexed, and search says
  so while coverage is incomplete.
- **SRCH-5** Some content (tests, changelogs, data files) is findable by keyword only.
- **SRCH-6** Looking back in time is a memory feature only. Past states of the code are
  not searchable.
- **SRCH-7** Questions that need several steps are answered by the caller's own agent
  looping over search and reads. inkentry has no question-answering command.

## Memory

- **MEM-1** Developers and agents add, list, show, tag, archive, supersede and link
  entries.
- **MEM-2** An entry can be created from a GitHub issue or a URL.
- **MEM-3** Every new entry is checked against existing ones, and likely duplicates and
  related entries are reported. Blocking a duplicate until the writer resolves it is
  opt-in.
- **MEM-4** Each entry is anchored to the commit that carries its work. Anchoring
  follows rebases and amends, and reports entries it cannot place instead of guessing.
- **MEM-5** Memory can be queried as of a point in time, and an entry's timeline can be
  shown.
- **MEM-6** Harvest proposes entries from commit history or agent session logs. It is
  the only feature that uses an LLM, and the LLM is one the user configures.
- **MEM-7** Tags are normalised. Linked files that no longer exist are flagged, not
  refused.
- **MEM-8** Memory can be imported from a portable dump.
- **MEM-9** A project's memory stays inside that project, except locked or
  cross-project decisions and requirements from linked projects. These are labelled
  with their source.

## Sharing

- **SHR-1** Memory travels with the repository through git. Teammates' entries appear
  after a fetch, with no setup.
- **SHR-2** A developer's own entries are published on push, once they have opted in
  by installing the hook. Nothing is published by default.
- **SHR-3** A team can instead share memory through a team server or inkentry cloud,
  kept in step by sync.
- **SHR-4** By default the local copy stays the working copy and the shared server
  converges with it. Making the shared server the copy of record is an explicit
  opt-in, and in that mode commands fail loudly when the server is unreachable.
- **SHR-5** Concurrent edits are kept, not overwritten. Contradictions are flagged for
  a person.
- **SHR-6** An operator can restore a team server after data loss from members'
  machines.

## Team server

- **SRV-1** One server is one trust domain: anyone holding its key can read and change
  every project on it. Teams that need isolation run separate servers.
- **SRV-2** A server reachable from other machines requires HTTPS and a key. The
  operator supplies the certificate.
- **SRV-3** inkentry does not run, host or supervise agents. Remote agents reach a
  server over HTTPS.

## inkentry cloud

- **CLD-1** A project opts into inkentry cloud. A project uses either a team server or
  the cloud, not both.
- **CLD-2** Developers sign in to the cloud from the CLI and can hold sessions for
  several organisations.
- **CLD-3** The cloud shares memory only. It holds no code and offers no code search.
- **CLD-4** Plans and quotas are set out on inkentry.com.

## Agents

- **AGT-1** Agents use the same commands as people, with machine-readable output.
- **AGT-2** The agent skill ships separately from the CLI, as an agent plugin.
- **AGT-3** Memory is not tied to any one agent harness.

## Promises

- **PRM-1** Code never leaves the developer's machine. Only memory is shared, and only
  through the channels in Sharing.
- **PRM-2** Context, keyword search, memory and the code graph work offline, with no
  account and no key. Concept search runs on the developer's machine, after a one-time
  model download.
- **PRM-3** The local search service never holds memory.
- **PRM-4** Outside a set-up project, commands refuse to run. There is no machine-wide
  store.
- **PRM-5** No telemetry. Usage metrics are kept locally, hold no query text or
  content, and can be cleared.
- **PRM-6** Credentials are kept in the operating system's keychain and are sent only to
  the server they belong to.
- **PRM-7** Secrets found in code or memory input are dropped. This is best effort, not
  a security boundary.
- **PRM-8** Hooks never block a commit or a push.
- **PRM-9** Command names, flags and exit codes are stable. Human-readable output is
  not.
- **PRM-10** Upgrades migrate in place. Downgrades are not supported.
- **PRM-11** Entries from teammates, servers or dumps are treated as untrusted data.

## Distribution

- **DIST-1** Distributed as GitHub releases, an install script, Homebrew, Scoop and
  Debian packages.
- **DIST-2** Supported platforms: macOS on Apple Silicon, Linux x86_64 and arm64, and
  Windows x86_64.
- **DIST-3** The latest release gets fixes. Older releases get critical security fixes
  only.

## Not the product

- **NON-1** Not an agent. It does not write or change code, review pull requests, or
  reason on the user's behalf. It retrieves and remembers.
- **NON-2** Not a replacement for grep, harness rules or style guides.
- **NON-3** No access control per project or per user within one team server.
