# Changelog

All notable changes to inkentry are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
inkentry uses [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

### Added

- **The cloud login session no longer lives in plaintext config, and each
  organization keeps its own.** `inkentry login` stores the WorkOS session
  (including the long-lived refresh token) in the OS secret store, keyed per
  organization. A legacy `[auth]` table in `~/.config/inkentry/config.toml` is
  migrated into the store and stripped from the file on first use, so nothing
  needs doing. A repo can pin itself to one org with `org = "<slug>"` in
  `.inkentry/config.toml` (or `INKENTRY_ORG`). `inkentry org list` shows the
  cached orgs, `inkentry org switch` between already-cached orgs is now local,
  and `inkentry logout --org <target>` signs out of one org while leaving the
  others.
- **Memory entries now show the id that travels with the repo.** `memory add`,
  `memory list`, `memory show` and `context` lead each entry with a 12-character
  handle taken from its entity id; `memory add` and `memory show` print the full
  value, and `--format json` and `--format jsonl` (now on `memory add` too)
  carry it as `entity_id` beside the unchanged `id`. `memory show`,
  `memory archive` and `memory supersede` accept the full entity id or any
  prefix of 8 or more characters, as well as the id they already took. Quote the
  entity id whenever you name an entry outside the machine that recorded it: the
  `id` beside it is minted per machine and does not travel. On a self-hosted
  team server a quoted handle resolves against the whole store, not only the
  most recent page.
- **GPU-accelerated embedding.** The embedder now runs on llama.cpp — GPU
  wherever a driver allows (Metal on macOS, Vulkan on NVIDIA/AMD/Intel on
  Windows and Linux x64) and CPU otherwise. Same vectors, same indexes —
  nothing re-embeds. `INKENTRY_EMBED_DEVICE=auto` (default), `gpu`, or `cpu`
  selects the device. The Windows/Linux-x64 archives now contain engine library
  files next to the binaries; keep the extracted files together when installing
  manually.
- **Faster CPU embedding on Linux arm64.** Where Vulkan can't go, the same
  llama.cpp engine runs on CPU, typically faster than the previous engine.
- **`inkentry search --quiet`** suppresses the informational notices on stderr.
  Results and exit codes are unchanged, and it never hides an error or the
  warning about a server started by another user. Reach for it in Windows
  PowerShell 5.1, which renders anything a native command writes to stderr as a
  red error block.

### Changed

- **The embedding engine is now llama.cpp** (it was candle); vectors and
  `MODEL_ID` are unchanged, so existing indexes and memory need no re-embed.
  Building from source now needs a C++ toolchain, `cmake`, and `libclang` —
  see `docs/building.md`.
- **The supported Linux floor is now Ubuntu 22.04 / Debian 12 (glibc 2.35).**
  Release binaries no longer target Debian 11; users on Debian 11 or Ubuntu
  20.04 should upgrade the OS or build from source.
- **BREAKING: a pushed memory vector must be near unit length.** Both memory
  write routes refuse a `vector` whose L2 norm is outside `[0.5, 1.5]` with
  `400`. Clients that compute their own vectors must L2-normalise before
  pushing, or omit `vector` and let the server embed. The `inkentry` CLI is
  unaffected.
- **Server setup now says what self-hosting costs.** `docs/server-setup.md` has
  a sizing section with the RAM and disk figures for a team server.
- **`inkentry plumbing graph-edges --file` now exits `2` for a path the index
  does not hold**, naming the path on stderr. It exited `1` before. Branch on
  `2` as an error; an indexed file with no edges still exits `1` with no output.
- **Source builds now parse PDF, DOCX and spreadsheets by default.** The
  `rich-formats` feature is on by default for `inkentry-cli`, so a plain
  `cargo build` matches every published binary and no longer needs
  `--features rich-formats`. To build the CLI without those readers, pass
  `-p inkentry-cli --no-default-features`.
- **The embedder runs one fixed context size on every machine**, so the same
  source embeds to identical vectors regardless of a host's RAM (it used to step
  down to a smaller context on lower-RAM machines and truncate longer inputs at
  a different point). A machine that cannot allocate the fixed context is
  refused at load with a clear message rather than silently degraded. Indexed
  code chunks are well under the cap and unaffected.

### Fixed

- **An interactive `search` or `memory add` no longer waits out a running
  index.** A query embed could take minutes while the server was busy embedding
  an index batch; it now returns straight away. Interactive embeds run on a
  reserved admission lane, so a bulk index pass can neither shed them nor make
  them wait.
- **The background indexing log is no longer empty.** `inkentry init` and
  `inkentry index --detach-embed` point at `index-background.log`, but the
  detached worker wrote nothing to it, so a run in progress looked exactly like
  one that never started. The worker now records when it started, how far the
  embedding has got, and the reason if it stops early. A new run appends to the
  log instead of overwriting what the last one reported.
- **A `cloud_first` command against an unreachable team server now fails in
  about two seconds instead of most of a minute.** The error names the server
  and says that `cloud_first` does not fall back to the local store. Reads and
  writes still never fall back.
- **A server with an untrusted certificate is no longer reported as
  unreachable.** The message names the certificate cause and points at
  `server_ca` instead of sending you to restart a server that is already up.
- **The server log file is plain text.** The daemon coloured its log
  unconditionally, so `server.log` held raw escape sequences. Colour is now
  emitted only when the log is going to a terminal, and `inkentry`'s own
  `RUST_LOG` output follows the same rule.
- **`inkentry hooks install` prints one path separator on Windows.** The
  installed-hook path read as `D:\src\repo\.git/hooks\post-commit`.
- **The model cache no longer holds the embedding model twice.** A fresh
  download now occupies about 350 MB instead of about 690 MB. If your cache was
  filled by an earlier release, delete the model cache directory and let the
  next server start refetch to reclaim the duplicate.
- **Interrupted model downloads are cleaned up.** A partial download is resumed
  where it left off, and one that nothing can resume is removed instead of
  sitting in the cache forever. A start that follows an interrupted download is
  no longer announced as a first run.
- **The documented model cache path is now correct on every platform.** It is
  the platform's local-data directory, which is not the same place as config
  and state. See
  [Where the model is cached](docs/getting-started.md#where-the-model-is-cached).
- **`relates_to`, `contradicts` and `supersedes` links now travel with the
  repository.** A clone rebuilds the graph the writer had, instead of the
  entries alone. Older builds still read the new records. Re-run
  `inkentry init`, or any command that reads memory, after a fetch to pick the
  links up. A link whose other entry has not arrived yet is reported as skipped
  and applied by a later import.
- **`inkentry memory add` no longer waits on a busy embedder.** Adding an entry
  during a bulk index pass could block for minutes, and the entry was not saved
  until the embed came back, so a caller that gave up first lost it. The entry
  is now saved first and the command returns in seconds. An entry saved before
  its search vector arrives says so, and is listed and readable straight away;
  `inkentry memory reindex` adds the vector, and the next `inkentry sync` does
  it on its own.

## [1.0.2] — 2026-09-03

### Added

- **A project can opt into inkentry cloud with `cloud = true` in
  `.inkentry/config.toml`.** `server_url` is for a self-hosted team server and
  the two cannot both be set.

### Security

- **The cloud access token is sent only to the host you logged in against.** It
  is bound to that host at login, so a `server_url` or environment setting can
  no longer direct it to a different origin.

## [1.0.1] — 2026-08-28

### Added

- **`inkentry plumbing push --force` recovers a team server that lost its
  database.** It re-offers every active memory entry, ignoring the local
  proof-of-sync, and restores each under its original identity; a healthy server
  skips whatever it still holds, so there are no duplicates. Use it to rebuild a
  self-hosted team server after a crash or dropped database. Other machines then
  pick the recovered entries up on their next `inkentry sync`, which now
  re-pulls the full dataset whenever the server reports more entries than it
  holds locally.

### Fixed

- **`inkentry login` now opens the correct hosted sign-in.** In 1.0.0 it pointed
  at a retired sign-in project, so logging in to inkentry cloud could not
  complete.
- **The embedder self-heals when macOS drops the Metal compiler service.** A
  long-running server no longer needs a restart to embed again. A request that
  hits the failure window gets a stable `embedder_device_lost` error, and
  `inkentry index` now says to retry instead of misblaming the request budget.

## [1.0.0] — 2026-08-24

### Internal

- **`cargo test -p inkentry-server --bins` no longer aborts the whole run.** A
  failed parse of an `INKENTRY_*`-bound arg now fails its own test instead of
  killing the harness. `cargo nextest`, which CI uses, never showed this.

### Added

- **The agent skill installs as a plugin**, from
  [inkentries/agent-plugin](https://github.com/inkentries/agent-plugin). In
  Claude Code: `/plugin marketplace add inkentries/agent-plugin`, then
  `/plugin install inkentry@inkentry`. It is packaged to the
  [Agent Plugins](https://agent-plugins.org/) standard, so clients implementing
  that standard can consume it too. Installing the plugin does not install the
  CLI.
- **Releases ship checksums and a signed build-provenance attestation.** Every
  release publishes a `SHA256SUMS` asset, and each archive and package carries
  an attestation binding it to this repository and commit. Verify with
  `gh attestation verify <file> --repo inkentries/inkentry`. Releases up to
  `v1.0.0-rc2` have neither.
- **`inkentry auth remove-key` removes a stored credential** (ADR-090): one
  server (`--server <url>`), all servers (`--all-servers`), or the LLM endpoint
  key (`--llm`). Removing a credential that is not stored exits 0.
- **Memory entries stored without a vector are repaired.** Entries stored while
  the embedder was loading, unavailable or disabled are now backfilled
  server-side. Each `/memory/batch` result gains an `embedded` boolean.
- **`GET /v1/health` advertises `accepts_pushed_vectors`.** A client can send a
  vector it embedded locally and skip server-side embedding, but only when the
  server advertises acceptance (`true` only while the embedder is ready with a
  known dimension). Absent on an older server, read as `false`.
- **Unified `search` over code and memory** (ADR-081). One `search` returns code
  chunks and memory entries interleaved into a single ranked list.
- **Corpus filters `--only-code` / `--only-memory` / `--only-text` on `search`.**
  `--only-text` runs full-text with no embedding and no server; `--only-code` and
  `--only-memory` are mutually exclusive.
- **Typed, nested `search` result envelope.** `search --format json`/`jsonl`
  emits one object per result with a `type` discriminator (`code`/`memory`) and
  the payload nested under a `code`/`memory` key; the human format labels each
  result `[code]`/`[memory]`.
- **Graph and cross-project attachments are unranked.** `--graph` neighbours,
  `--expand-graph` neighbours and cross-project entries are appended after the
  ranked members with `fused_rank`/`fused_score`/`corpus_rank` all `null`.
- **`--as-of <date>` and `--expand-graph` work on `search`**; `--local-only`
  disables the cross-project dependency pass on both corpora.
- **`inkentry import <dump>`** reads a [portable dump](docs/dump-format.md) —
  memory entries, relationships, projects and recorded commands — into a store
  this build created; nothing is opened in place. The dump is verified whole
  before anything is written. Embeddings are not carried, so import runs `memory
  reindex`'s pass and reports how many entries await embedding (`--no-embed`
  skips it). Refuses to run under `cloud_first` with a `server_url`, naming the
  recovery path.
- **Imported memory entries travel with the repository.** `inkentry import`
  appends what it landed to `refs/notes/inkentry` (`store_in_git_notes = false`
  turns it off), so a teammate cloning the repo gets the imported decisions.
  Known limitation: a supersede edge reaches a clone's carrier but not its
  `memory.db`.
- **Deterministic structural chunk summaries in the built-in tier** (ADR-080).
  Each chunk's `summary:` embedding slot is composed offline — no model, no key,
  no network — and is byte-identical across runs. It is secret-scanned before
  storage.
- **Title-less chunks (Markdown sections, oversized windows) get an
  MMR-selected summary**, chosen deterministically with no whole-chunk re-embed.
- **`inkentry status` reports the embedding backlogs.** `memory_embedding_pending`
  counts entries not yet in semantic search and names `inkentry memory reindex`.
  `status --format json` also gains `embedding_refresh_pending` and
  `summary_scheme`, and `search` no longer prints a bare "No results found." over
  a still-refreshing index.
- **`inkentry context` surfaces active agent sessions at session start.** It
  lists other live `intent` entries and warns per file the current worktree has
  modified that an active intent also claims. JSON output (and `AGENT=true`) gains
  an `intent` section and a top-level `overlaps` array.
- **`inkentry harvest` is a top-level command**, with full flag and source parity
  (`--git-range`, `--branch`, `--source`, `--batch-size`, `--history-file`,
  `--since`, `--confirm`, `--detach`, `--db`, `--backend`). The post-commit hook
  and the `--ci` snippet install `inkentry harvest`.
- **`inkentry plumbing push` and `inkentry plumbing pull`** — one-way memory
  transfer as plumbing, for seeding a team server or running in CI. Each emits a
  single JSONL report and follows the plumbing exit-code contract (an empty delta
  exits `1` but still emits the report). Both require an explicitly-configured
  team `server_url`.

### Changed

- **BREAKING: `inkentry plumbing push` exits `2` when there is no local memory
  store**, instead of creating one and reporting an empty delta. Run
  `inkentry init` first, or point `--source` at a real `memory.db`.
  `plumbing pull` and `sync` still create the store.
- **BREAKING: the default server port is 4655** (team convention 4658), off the
  registered 7777/7778. Loopback and team users need do nothing — the port comes
  from `server.port` or an explicit `--port`, not a shipped default. Adopting the
  new unit files means moving the server and its clients' `server_url` together.
- **An auto-started daemon whose port is taken now takes an ephemeral port**
  instead of walking the ports above it. Explicit `inkentry server start` still
  binds `--port` exactly and fails loudly if it is held.
- **BREAKING: the memory write routes take `vector` / `vector_model` /
  `vector_precision`** in place of a bare `embedding`. The `embedding` field is
  removed with no alias, so a request still sending it is embedded server-side. A
  vector must arrive with both `vector_model` (the server's model) and
  `vector_precision` (`fp32`), or the write is refused with `400`.
- **BREAKING: a store written by an older build is not opened in place, and there
  is no migration path.** `index.db` is discarded and rebuilt empty — re-index
  with `inkentry index .`; `memory.db` is refused with a pointer to
  `inkentry import`. Export first with `spelunk-export` before upgrading from
  0.9.8, then `inkentry import` the dump — see [Upgrading](docs/upgrading.md). A
  store from a newer build is refused and left untouched.
- **BREAKING: every memory-entry id is a UUID string** (ADR-078). `id` and
  `superseded_by` are JSON strings everywhere they appear, and `memory graph`,
  `memory add --relates-to` and `plumbing read-memory --id` take a UUID. Old
  numeric ids no longer resolve (answered with a pointer to `inkentry memory
  list`), so a script parsing an integer `id` must be updated.
- **BREAKING: a self-hosted `inkentry-server` identifies memory entries by
  UUIDv7**, and every route carrying a note id now speaks strings (the OpenAPI
  document is regenerated to match). Servers upgrade in place — existing entries
  are assigned an identity on the next start — but a client holding an integer id
  from an older server cannot resolve it.
- **BREAKING: `search --format json`/`jsonl` is the nested code/memory envelope,
  not a flat `SearchResult[]`** (ADR-081). Consumers that parsed the top-level
  array, or re-sorted by `distance`, must move to the emitted order / `fused_rank`;
  per-corpus `distance`/`score` are not comparable across corpora.
- **`search` is no longer a pure read.** Folding in the memory corpus brings the
  git-notes refresh with it, so a plain `search` may run `git notes merge` and
  update `memory.db` when a teammate's entries have arrived. It is OID-gated and
  does nothing when nothing changed.
- **`search` requires an index.** An uninitialised directory funnels to
  `inkentry init`. Full-text results are available once `init` has parsed the
  tree; semantic ranking builds in the background.
- **PageRank runs before the embed phase**, so a cold first index embeds
  PageRank-central code first. Structural summaries are composed in the same pass.
- **A chunk whose embedding input changes is re-embedded in place.** Every
  existing vector is kept until its replacement lands, so semantic search coverage
  never falls to zero while a refresh drains.
- **Memory entry relationships are stored by entry id**, and the memory store
  enforces foreign keys itself: an edge naming an entry that does not exist is
  refused.
- **The `external_id` the CLI mints when pushing to the hosted API is a UUIDv7**
  rather than a v4.
- **Team memory sharing has a single everyday verb: `inkentry sync`** (two-way
  convergence). Surface change only — `sync`'s behaviour, the sync modes, the wire
  protocol and every server route are unchanged.
- **Install and package paths are under the `inkentry` name:** install script
  `curl -fsSL https://get.inkentry.com/install.sh | sh` (PowerShell:
  `irm https://get.inkentry.com/install.ps1 | iex`), Homebrew
  `brew install inkentries/inkentry/inkentry`, Scoop
  `scoop bucket add inkentry https://github.com/inkentries/scoop-inkentry`.
- **BREAKING: a plaintext `http://` team server is refused even when no
  credential is configured.** The transport guard now runs on every remote sync
  client, not only when a bearer is present. If you run a team server over plain
  `http://` with no bearer, put TLS in front of it or bind it to loopback. TLS
  and loopback servers are unaffected.

### Deprecated

- **`inkentry memory harvest` is a deprecated alias of `inkentry harvest`.** It
  still works and produces identical results, printing a one-line deprecation
  warning, so post-commit hooks installed before this change keep working until
  re-installed. Removal is a later release.

### Removed

- **BREAKING: `inkentry logout --servers` and `inkentry logout --server <url>`**
  (ADR-090). Use `inkentry auth remove-key --all-servers` or
  `--server <url>` instead. Bare `inkentry logout` is unchanged.
- **BREAKING: a `server_key` in `~/.config/inkentry/config.toml` is no longer
  read or migrated forward** (ADR-088). Run `inkentry auth set-key --server <url>`
  once per server. A config file still carrying the line is named on stderr —
  rotate that key rather than moving it.
- **The `--mode` flag on `search`, the top-level `graph` command, and
  `memory search`** (ADR-082). All three exit `2` naming the replacement and are
  absent from `--help`:
  - `--mode text` → `--only-text`; `semantic`/`hybrid`/`auto` → the default;
    `ast-grep` → no replacement.
  - `graph` → `search <symbol> --graph`, or `plumbing graph-edges --symbol
    <name>` / `--file <path>`. `memory graph` is a different command, unaffected.
  - `memory search` → `search --only-memory`, with `--as-of` / `--expand-graph` /
    `--local-only` carried over.
- **The in-process ast-grep structural-search engine** and the `ast-grep-core`
  dependency. Tree-sitter grammars (`ast-grep-language`) that back
  parsing/chunking stay.
- **LLM-generated chunk summaries.** `inkentry index` no longer calls an LLM;
  `--summary-batch-size` is gone and `--no-summaries` now skips the structural
  pass. For abstractive summaries, run your own agent over `inkentry plumbing
  cat-chunks` (see `docs/examples/abstractive-summaries.md`). `harvest` is now the
  only LLM-backed feature.
- **`inkentry check`.** Use `inkentry index` for freshness (blake3-gated),
  `inkentry server status` / `inkentry status` for health, and `inkentry context`
  for intents and overlap. The file counts and `last_indexed_at` / `memory_backend`
  fields move to `inkentry status --format json`, and the stale-file list is
  `inkentry plumbing ls-files --stale` — note its inverted exit polarity (`0` when
  stale files exist), so a "fail if stale" gate must test for output, not a
  non-zero exit.
- **`inkentry explore` and the `POST /v1/projects/{project_id}/explore` route**
  (ADR-079). Multi-hop exploration is now a skill your agent runs over `search`,
  `graph` and `chunks` (see `SKILL.md`). A server no longer advertises an
  `explore` capability in `/v1/health`, and `status --format json` drops the
  `explore` key from `usage_7d`.
- **`inkentry memory push`, `inkentry memory pull`, `inkentry memory watch` and
  `inkentry memory since`.** One-way transfer is now `inkentry plumbing push` /
  `pull`; two-way is `inkentry sync`. Streaming and point-in-time queries return
  with the cloud product. No aliases. The server's `/memory/since` and
  `/memory/stream` routes are unchanged.
- **The memory store's schema-migration ladder** and its two on-open backfill
  routines. The store is created at its final shape.

### Fixed

- **The background embed worker no longer stalls for ~2.5 minutes when you have
  opted out of a server.** Under `INKENTRY_NO_SERVER=1`, `INKENTRY_MODE=offline`
  or `mode = "offline"`, it now returns at once and prints the skip notice.
- **A piped `inkentry init` or `inkentry server start` returns on Windows**
  instead of blocking until the background worker exits. Only pipes were affected
  (CI, agent harnesses, `| tee`, `$(...)`); terminals, macOS and Linux were not.
- **Entries arriving from `inkentry sync` or `inkentry plumbing pull` are
  embedded locally**, so a teammate's entry is findable by semantic search. With
  no embedder reachable the pull still succeeds and reports how many are pending.
  `plumbing pull`'s report gained `embedded_locally` and `without_local_vector`.
- **`inkentry status` says why it is offline** instead of always suggesting
  `server_url`. It names whichever setting is in force, or points at
  `inkentry server start`.
- **`inkentry search` and `inkentry index` say why semantic ranking is off**
  instead of reporting every case as "no server running", with the same remedy
  `inkentry status` gives.
- **`plumbing push`, `plumbing pull` and `plumbing read-memory` act on the same
  memory store the `memory` commands do.** In a configured-but-unindexed project
  they previously acted on the machine-global store; a linked worktree now shares
  the main worktree's store.
- **Outside any project, the plumbing memory commands name the global store they
  act on** on stderr; stdout stays the JSONL report alone.
- **`plumbing read-memory` exits 2 when no memory store exists**, rather than
  reporting no entries.
- **Docs: the memory-sharing claim is corrected in the remaining four places.**
  `docs/memory.md`, `docs/commands.md`, `SKILL.md` and `SECURITY.md` each still
  said memory travels with the repository. `git push` does not push
  `refs/notes/*`, so entries stay local until the pre-push hook is installed.
- **Docs: `SKILL.md` notes the plumbing exit-code contract where the commands are
  recommended**, since a plumbing verb exits 1 on an empty result.
- **`mode` in `.inkentry/config.toml` now takes effect.** The project config
  previously read only four keys, so `mode = "cloud_first"` written alongside
  `server_url` silently ran `local_first`. `mode` is now read from either file
  (project wins over personal; `INKENTRY_MODE` over both).
- **A `server_key` line in `.inkentry/config.toml` now warns and says to
  rotate**, instead of being dropped in silence. The field is still never read
  from a committed file.
- **`llm_url` is now a project config key**, so a team can state its provider once
  (project wins over personal; `INKENTRY_LLM_URL` over both). The credential does
  not follow it into either file.
- **A key `.inkentry/config.toml` is not read for is named on stderr** instead of
  dropped in silence. This is a warning, never a refusal: the rest of the file
  loads and the command runs.
- **An index rebuilt for a new schema now says so**, instead of reading as an
  empty repository. The rebuild keeps recorded usage; `search` and `status`
  attribute the emptiness until a re-index, and `status --format json` carries the
  discarded version as `index_rebuilt_from`.
- **Embedding no longer collapses to a single thread on 2-core hosts** (#112).
  When the thread budget resolves to 1, the log and `inkentry status` name
  `INKENTRY_EMBED_THREADS`, and `/v1/health` reports `limits.embed_threads`.
- **The embedder sizes its single-chunk budget to the host's real RAM on
  Windows** (#124). Windows had no RAM detection, so the embedder always fell
  back to a conservative 2 GiB budget regardless of the machine; the startup log
  now reads `unknown` instead of a false `0.0 GiB` when detection fails.
- **`hooks install` no longer promises harvesting it cannot do.** With no LLM
  reachable it says harvesting stays inactive until one is. The hook installs
  either way; configuring an LLM later needs no reinstall.
- **`--detach` no longer converts every failure into silence.** A detached run's
  stdout and stderr now append to `.inkentry/background.log` under a header naming
  the run and its time.
- **The post-commit hook no longer does nothing when `inkentry` is off `PATH`.**
  It now embeds the installing binary's absolute path. A hook whose binary was
  removed exits 0 rather than failing the commit; re-run `inkentry hooks install`
  to re-resolve a moved binary.
- **A server key file saved with a UTF-8 BOM no longer makes every request fail
  with 401.** A leading BOM is stripped from every key source (`--key`,
  `--key-file`, `INKENTRY_SERVER_KEY`, the systemd credential), and a resolved key
  still holding a byte an HTTP header cannot carry refuses startup, naming its
  source.
- **Docs: memory does not "travel with the repo" by default**, contrary to the
  getting-started guide and README. Publishing your own notes is opt-in via
  `inkentry hooks install --pre-push`; only the fetch side is automatic. Both now
  say so, getting-started documents `--pre-push` and the two `.git/config` entries
  `init` writes, and the supported-language lists note that `inkentry languages`
  is build-dependent (`rich-formats` adds DOCX, spreadsheets, PDF).
- **Docs: Jupyter notebooks are now listed as supported.** `notebook` has always
  been parsed but neither the README nor CLAUDE.md named it; both now do, and the
  README regained `text`. The README's Markdown-chunking and skipped-file-type
  claims are corrected too.
- **`status` no longer misreads a foreign pid as a live embed worker when the
  project path contains "inkentry" and "index".** The liveness check now requires
  the pid's own binary to be `inkentry` and an actual `index` subcommand token;
  the Windows `tasklist` path is tightened the same way.
- **The local relay's idle-retirement can no longer orphan a session a request
  just touched**, stalling background sync.
- **A daemon started from a shell now survives that shell exiting.** On Unix the
  spawned `inkentry-server` stayed in the terminal's session, so closing it
  SIGHUPed the server. Windows detachment is unchanged.
- **`inkentry server stop` no longer implies nothing is running when the pid file
  is missing.** It now distinguishes "this CLI has no record" from "nothing is
  running" and says how to find the process.
- **A memory push no longer stalls every other request on the server.** One
  consequence: a batch's embedding call is all-or-nothing, so a text that fails
  the embedder leaves that request's entries stored text-only; `inkentry memory
  reindex` backfills them.
- **A busy server no longer aborts a sync.** `inkentry sync`, `inkentry plumbing
  push` and `inkentry memory add` now honour the server's `Retry-After` on a `429`
  and retry a bounded number of times, then report and ask for a re-run that
  resumes from the entries that had not landed.
- **`search` returns the same order for the same query.** The fused order is now
  total and deterministic, tie-broken on content properties — path plus line span
  for code, the entry UUID for memory — so two machines that indexed the same tree
  agree, including on a partially embedded index.
- **`import` no longer implies text search covers the entries it could not
  embed.** The completion message and `docs/commands.md` now name what actually
  reaches them: `memory list` and `context` (which take no query), and
  `inkentry memory reindex` to finish the job.
- **Team memory converges in the background against a server with an internal
  CA.** The daemon's relay now builds both its catch-up and streaming HTTP clients
  with the configured `server_ca`.

### Security

- **Loopback auto-discovery verifies a responder before making it the embedding
  backend**, so a local process squatting the recorded port can no longer receive
  your indexed code. **Restart your daemon after upgrading** (`inkentry server
  stop`, then `start`): one started by an earlier build recorded no instance id
  and is not rediscovered until restarted.
- **The local relay no longer hands memory entries or your team credential to an
  unverified local listener.** It now checks the responder is the daemon this CLI
  recorded. **Restart your daemon after upgrading** so its instance id is
  recorded.
- **`inkentry harvest`'s git-commit walk secret-scans each commit message**,
  skipping a matching commit (warning with its SHA only) and continuing, instead
  of promoting it into memory and `refs/notes/inkentry` unchecked.
- **Bumped `h2` to 0.4.16** for RUSTSEC-2026-0258 (unbounded empty DATA frames).
- **The local relay no longer opens outbound connections to a caller-chosen
  host.** `POST /local/relay/push` previously took `server_url` and `bearer` from
  the request body and connected there; a request now only *selects* among the
  team servers this machine already declares. The relay routes are unmounted on a
  non-loopback bind, and relay sessions are capped and retired when idle
  (ADR-056).
- **The `/llm/complete` rate limit can no longer be lifted by setting a header.**
  The bucket key used the caller's own `X-Forwarded-For`; it now comes from the
  TCP peer. An operator running a proxy opts in with `--trusted-proxy` (or
  `INKENTRY_TRUSTED_PROXIES`), and only the trailing forwarded entry is read.

---

Releases before 1.0.0 shipped under this project's predecessor, spelunk. That
history is not repeated here; it is kept in the predecessor repository's
changelog, at
[spelunk-cloud/spelunk](https://github.com/spelunk-cloud/spelunk/blob/main/CHANGELOG.md).
