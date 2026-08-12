# Stability contract

This document says which parts of inkentry you may build on, and what a version
bump is allowed to do to them.

inkentry follows [Semantic Versioning](https://semver.org/). The surfaces below
are frozen against this document: a breaking change to any of them requires a
major version bump.

Every surface is one of three things:

| Level | Promise |
|---|---|
| **Stable** | Frozen for the life of a major version. Changes are additive only. Anything removed, renamed, or retyped requires a major bump, after a deprecation period. |
| **Best-effort** | Changes are avoided and announced in the changelog, but a minor release may still change them. Depend on it if you must; pin your version if you do. |
| **Internal** | No promise at all. May change in any release without notice. Do not build on it. |

If a surface is not listed here, treat it as **internal**.

## What is not stable

Stating this first, because it is the part most often assumed:

- **Porcelain output.** The human-readable text of `search`, `status`, `context`,
  `memory`, and every other non-plumbing command. Colours, column
  widths, wording, ordering, and summary lines all change freely. Parsing them
  with `grep`/`awk` will break. Use the plumbing commands instead, or one of the
  structured `--format` modes covered under
  [Structured output from porcelain commands](#structured-output-from-porcelain-commands).
- **Log and tracing text.** Message wording, `tracing` targets, span names, and
  log levels. Diagnostics on stderr from any command, including the diagnostics
  that accompany a plumbing exit 2, are advisory text and not a parseable
  interface. The *exit code* is the interface.
- **Internal crate APIs.** `inkentry-core`, `inkentry-cli`, `inkentry-embed`, and
  `inkentry-server` are workspace members, not published to crates.io. Any Rust
  item they expose, `pub` or not, may change in any release. Depending on them
  as a git dependency is unsupported.
- **The `/local/` HTTP routes.** `inkentry-server` registers `/local/relay/push`,
  `/local/relay/poll`, and `/local/relay/ack` outside the documented API. They
  are deliberately absent from `docs/openapi.json` and are internal transport
  between a inkentry client and its own server. They are served only on a
  loopback bind, and only for the team servers the machine's own configuration
  declares: a request selects among those, it cannot name a new one.
- **Wire-format details of the index.** Embedding vectors, the exact SQL schema
  of any table, and sqlite-vec internals. The *file* is a compatibility surface
  (see On-disk formats below); the SQL inside it is not.

## CLI

**Stable:** command names, subcommand names, flag names (long form), positional
argument order, and exit codes, for every command listed in `inkentry --help`.

- New commands and new flags may be added in a minor release.
- A flag's default value may change only in a major release.
- Short flags (`-d`) are **best-effort**: they are stable in practice, but a
  collision with a new long flag may force a reassignment.
- Hidden flags (clap `hide = true`, for example `publish-notes`' positional
  remote URL) are **internal**, present only for compatibility with the callers
  that pass them.

### Exit codes

The plumbing exit codes are the most load-bearing part of the CLI contract,
because scripts branch on them. They are **stable**:

| Code | Meaning |
|---|---|
| `0` | Succeeded, one or more results emitted on stdout. |
| `1` | Succeeded, no results. An empty set, **not** an error. |
| `2` | Hard error. Diagnostics on stderr, **stdout is empty**. |

A script must distinguish `1` from `2`. Treating any non-zero exit as fatal is
wrong: `1` means the query was valid and matched nothing.

Three commands cannot return `1`, by construction, and this is part of the
contract rather than an oversight:

| Command | Why it never returns 1 |
|---|---|
| `hash-file` | A readable file always has a hash, so there is always exactly one result. |
| `embed` | Empty stdin is an empty *input*, not an empty result set. It exits `0` having emitted nothing. |
| `publish-notes` | It runs from a `pre-push` hook, where a non-zero exit aborts the user's branch push. Both "nothing to publish" and, under `--best-effort`, a publish failure exit `0` and report the outcome in the JSON payload. |

`push` and `pull` reach exit `1`, but with one deliberate difference from the
rule above: they emit their one report object on every completed run, so their
exit `1` means the run completed with an **empty delta** (nothing new pushed, or
nothing new pulled) and stdout still carries the report. Only their exit `2` —
the run did not complete — leaves stdout empty. This keeps a machine consumer
able to read the outcome of an empty run instead of guessing from a bare exit
code, while the "stdout empty on `2`" guarantee that scripts rely on is intact.

Porcelain commands use `0`/`1` with their own documented meanings and do not
follow the plumbing convention.

### Structured output from porcelain commands

Most porcelain commands take a `--format` flag that switches stdout from the
human-readable text above to a machine-readable shape: `json` everywhere,
plus `jsonl` on `search` and `memory list`. This is a **different
surface** from the text output, and a different one again from plumbing JSONL:
none of it is covered by the plumbing golden schema.

`search --format json`/`jsonl` emits per-corpus envelopes (`{type, fused_rank,
fused_score, corpus_rank, code|memory}`); see
[`inkentry search`](commands.md#inkentry-search).

| Surface | Level |
|---|---|
| `inkentry status --format json` | **Stable** for its core fields, on the same additive-only terms as plumbing JSONL: new optional fields may appear, existing ones are not renamed or removed, and consumers must tolerate unknown fields. The field list is documented on the `status` handler in `crates/inkentry-cli/src/cli/cmd/status.rs`. |
| Every other `--format json` or `--format jsonl` mode | **Best-effort**. Structured, and reasonable to script against, but not enforced by a golden schema. Changes are avoided and go in the changelog; pin your version if you depend on the exact shape. |

`status --format json` also emits a set of richer fields for tooling (`tier`,
`mode`, `sync_pending`, `sync_last_synced_at`, `server_url`, `capabilities`,
`embedder_state`, `embedding_count`, `embedding_pending`, `embed_worker_alive`,
`embed_tokens`, `drift_candidates`, `usage_7d`) that are explicitly **not** in
the stable set and may change or disappear in a minor release.

If you need a surface with a test-enforced schema, use the plumbing commands.

## Plumbing JSONL

**Stable:** for every `inkentry plumbing <command>`, the *name and type* of each
field in the emitted JSON objects, and the guarantee that stdout is newline
delimited JSON with exactly one object per line.

Not stable: field **order** within an object, the **number** of lines, and the
**values** themselves (line numbers, hashes, scores, and timestamps all move).

### Evolution rule: additive only

Within a major version:

- **Allowed:** adding a new field. A consumer that ignores unknown fields is
  unaffected, and every consumer is expected to ignore unknown fields.
- **Not allowed:** removing a field, renaming a field, changing a field's JSON
  type (including widening an integer to a float), or making a
  previously-always-present field conditional.

A field documented as optional may legitimately be absent. Those are fields the
serializer skips when unset; they are listed as `optional` in the golden schema
described under [Enforcement](#enforcement).

## Server HTTP API

**Stable:** every route under `/v1/`, as described by `docs/openapi.json`. That
covers paths, methods, request and response schemas, and status codes.

Within `/v1/`:

- **Allowed:** new routes, new optional request fields, new response fields, new
  enum values in a field documented as open.
- **Not allowed:** removing a route or method, removing or renaming a response
  field, making an optional request field required, narrowing an accepted type,
  or changing the meaning of a status code.

Anything outside `/v1/` is internal. `GET /api-docs/openapi.json` serves the
spec from the running binary and is **best-effort**: useful for tooling, but not
a route to build a product on.

`info.version` inside the spec is a placeholder and does not track the crate
version. Use `GET /v1/health` for the server's real version.

## Config

**Stable:** the key names, types, and defaults documented in
[Config reference](config-reference.md).

**Also stable, and just as load-bearing: which file a key may be set in.** A key
is not simply "supported"; it is supported in a specific place. Three keys are
called out by name, because each is one a reader would otherwise reasonably
guess wrong about, and each restriction is part of the contract:

- `server_url` is **ignored in the global personal config**
  (`~/.config/inkentry/config.toml`, including a file passed to `--config`). It
  may come only from the checked-in `.inkentry/config.toml` or from
  `INKENTRY_SERVER_URL`. Everyone working on a project needs the same team
  server, which a per-developer file cannot guarantee. A global config that
  still sets it loads fine; the value is discarded.
- `server_key` is **ignored in the project config** (`.inkentry/config.toml`). A
  repository must never be able to hand a secret to whoever clones it. Use
  `inkentry auth set-key --server <url>`, `inkentry login`, or
  `INKENTRY_SERVER_KEY`.
- `llm_url` is **ignored in the project config**, which follows from the
  allowlist below rather than being an exception to it, and is named here
  because it looks like `server_url` and is not. An LLM endpoint is a
  per-developer choice: a committed value points every teammate's local daemon
  at whichever machine the author was running a model on. Set it in the
  personal config or via `INKENTRY_LLM_URL`. Its credential is not a config key
  in either file (`inkentry auth set-key --llm` or `INKENTRY_LLM_KEY`), on the
  same reasoning as `server_key`.

Beyond those three:

- Unrecognised keys are ignored rather than rejected. A config written for a
  newer inkentry still loads on an older one, and a config carrying a removed key
  still loads. A key ignored because it is in the wrong file behaves the same
  way: the rest of the file is unaffected.
- The **project-level allowlist** is itself stable. A checked-in
  `.inkentry/config.toml` is honoured for exactly `server_url`, `project_id`,
  `server_ca`, and `[index]`. Adding a key to that allowlist is additive and
  allowed; removing one is a breaking change.
- Environment variable overrides (`INKENTRY_*`) are stable on the same terms as
  the keys they override. They are not subject to the file restrictions above:
  `INKENTRY_SERVER_URL`, `INKENTRY_SERVER_KEY`, and `INKENTRY_LLM_URL` all take
  effect wherever they are set. What a variable set to an **empty** value does
  is documented in [Config reference](config-reference.md) but is not frozen
  here.

### Deprecation policy

Removing or renaming a stable config key follows a fixed sequence:

1. **Alias.** The old key keeps working, mapped onto the new one, for at least
   one full minor release.
2. **Warn.** While the alias still works, using it emits a deprecation warning
   on stderr naming the replacement. The warning lives and dies with the alias:
   once the key is gone there is no warning, because a load-time message whose
   only job is to describe a key that no longer does anything is permanent code
   for a one-release problem. See
   [ADR-071](adr/071-per-server-client-bearer-scoping.md) for the reasoning.
3. **Remove.** The key is dropped in the next major release and recorded under
   `### Removed` in the changelog. It then falls back to the ignore-unknown-keys
   rule, so existing configs still load; the key simply stops having an effect.

The same three steps apply to CLI flags and to `/v1/` request fields.

## On-disk formats

The promise here is **that no recorded memory is lost**, and that a store this
build cannot read is refused or rebuilt rather than opened and damaged. The
promise is *not* that the SQL schema stays fixed, and from 1.0.0 it is also
**not** that an upgrade never requires you to move your data across: neither
store migrates, and the one that holds authored data has to be exported and
imported. See [Upgrading](upgrading.md) for the sequence, which is a team-wide
one rather than a personal one.

| Store | Versioning | Level |
|---|---|---|
| `.inkentry/index.db` | `PRAGMA user_version`, no ladder | **Stable**: a store this build did not write is discarded and rebuilt empty, carrying the `usage` table across, and one from a newer build is refused. The index is derived from your source tree, so `inkentry index` is always a valid recovery. |
| `.inkentry/memory.db` | `PRAGMA user_version`, independent of the index, no ladder | **Stable**, and stricter: memory is authored and cannot be rebuilt, so a store this build did not write is refused outright and left untouched on disk. An older one is refused with a message naming the export and [import](commands.md#inkentry-import) path; a newer one is refused with a message to upgrade. |
| `~/.config/inkentry/registry.db` | none | **Best-effort**. Tables are created idempotently. It holds project registrations, which are re-derivable by re-registering. |
| git notes on `refs/notes/inkentry` | `schema_version` inside each JSON record | **Stable**. A record with a higher `schema_version` than the reader knows is refused rather than misread, and lines that are not inkentry records are left untouched, so the ref can be shared with other tooling. |
| server-side database | sequential migration files | **Internal** to a server deployment, and not a client-facing surface. |
| [portable dump](dump-format.md) | `format_version` in the header record | **Stable**. Version 1 stays readable for the life of the major version; change within a version is additive only, and anything a version 1 reader could not handle is a version bump. A dump is refused whole rather than partially read, so an unreadable one never turns into a partial import. |

There are no migrations. Downgrading inkentry after an upgrade is not
supported, and the next section says what each store actually does when you try
it anyway.

### Downgrading, and what each store does

"Not supported" does not mean "prevented", so it is worth being exact about the
two stores, which behave differently in both directions.

**Both refuse a store from a newer build.** The stamp is compared against the
opening build's own constant, and anything above it stops with a message to
upgrade rather than being opened. The file is left as it was.

**Below its own stamp, the two diverge.** `index.db` is discarded and rebuilt
empty, carrying only `usage`; `memory.db` is refused and left untouched, with
its message naming the export and import path. That is the whole of the
compatibility behaviour: neither store is ever converted in place.

Each store's constant sits **above** the highest `user_version` its old
migration ladder ever stamped, and nothing may reclaim that range. `PRAGMA
user_version` is one integer per file, shared with every stamp those ladders
wrote, so a numbering that restarted at 1 would make a store from an older
release read as one from a *newer* build, and be refused with advice to upgrade
to something that does not exist. Both constants are asserted against that
bound at compile time.

An older release opening a current store is the case with no guard on this
side, because those binaries are frozen. A released build that still carries a
ladder will read a current store as one it can migrate forward, since its own
`CURRENT_SCHEMA_VERSION` is below the current stamp, and will re-stamp the
`user_version` down to its own on the way. It is not corruption in itself, but
it is not a supported configuration and the current build's answer to what it
leaves behind is to rebuild (`index.db`) or refuse (`memory.db`) like any other
store it did not write.

### `.inkentry/` layout

**Stable:** the directory name `.inkentry/` at the project root, and the names
`config.toml`, `index.db`, and `memory.db` within it. Tooling may rely on
`.inkentry/index.db` marking a project root, which is how inkentry itself
discovers one.

**Internal:** everything else in that directory, including lock files, pid
sidecars, and background logs. Names, formats, and existence may change, and an
internal file may be removed outright.

`~/.config/inkentry/` (config and registry) and `~/.local/state/inkentry/`
(runtime state for the local server) follow the same split: the config file is
stable, the state files are internal.

## Enforcement

A contract nothing checks is a wish. Each promise above is tied to something
that fails CI when it is broken.

| Promise | Enforced by |
|---|---|
| Plumbing JSONL field names and types | `crates/inkentry-cli/tests/golden/plumbing_jsonl_schema.json` plus `crates/inkentry-cli/tests/plumbing_jsonl_contract.rs`. Each command is run for real and its output checked against the committed schema. Required fields must be present and correctly typed; **undeclared fields are accepted**, so additive change passes and removal, rename, or retype fails. |
| Every plumbing command has a declared schema | `golden_schema_covers_every_plumbing_subcommand`, which reads the command list out of clap's own help, so a newly added command cannot ship as an unguarded stable surface. |
| The checker itself actually rejects things | `crates/inkentry-cli/tests/schema_contract_checker.rs`. Without it, a checker that accepted everything would leave every golden file green. It drives removal, rename, and retype across every field of every declared command, and pins the reporting wrapper too, including its refusal of a command that emitted no rows at all. |
| Each declared field is load-bearing, per command | `assert_every_declared_field_is_load_bearing`, run inside every command's conformance test in `plumbing_jsonl_contract.rs`. The command's real output is replayed with one declared field dropped, then retyped, and the checker must object each time. Conformance alone would pass against a checker that never rejects anything. |
| Plumbing exit codes 0/1/2 | `crates/inkentry-cli/tests/plumbing_exit_codes.rs`, covering all three codes for every command, including the stdout-is-empty guarantee on exit 2 and the three documented exceptions. |
| `/v1/` matches `docs/openapi.json` | The `openapi-snapshot` job in `.github/workflows/ci.yml`. The spec is generated from the running binary (`cargo run -p inkentry-server -- --print-openapi`) and diffed against the committed file, so a route or schema change that skips regenerating the snapshot fails CI. |
| The git-notes record format, across every era that wrote it | `crates/inkentry-cli/tests/upgrade_corpus.rs`, run by `.github/workflows/upgrade-corpus.yml`. A ref written by **real released binaries** across three writing eras is read with the current build and checked entry for entry, including that the era which wrote every entry twice is folded rather than surfaced twice. Every other migration test in the repo builds an old shape by hand, which tests what we believe the old format was; this one tests what it is. The same suite carries a tripwire that fires when a store's schema version advances past what the corpus covers, so a release whose databases *do* need to survive an upgrade cannot ship without one being captured. See [the upgrade corpus](../scripts/upgrade-corpus/README.md). |
| Neither store's version constant reclaims a range an old ladder stamped | A compile-time assertion in each store's module, against the recorded highest legacy stamp. Reclaiming it would make an older release's store read as one from the future, and be refused with advice to upgrade to a build that does not exist. |
| The above run on every change | `.github/workflows/stability-contract.yml`. |

### Changing a stable surface deliberately

If a change to a stable surface is intended:

1. Confirm it is additive. If it is, the golden schema needs no edit, and the
   tests already pass.
2. If it is not additive, it is a breaking change. It needs a major version, a
   deprecation period first, and a changelog entry under `### Removed` or
   `### Changed`.
3. For the server, regenerate the spec:
   `cargo run -p inkentry-server -- --print-openapi > docs/openapi.json`.
4. Update the golden schema and this document in the same change, so the
   contract and the code never disagree.

## What's next

- [Version skew](version-skew.md): what happens when the two ends of a
  connection are different versions
- [Plumbing and porcelain](plumbing-and-porcelain.md): why the split exists and
  how to script against it
- [Commands](commands.md): the full CLI reference
- [Config reference](config-reference.md): every key, default, and env override
- [Releasing](releasing.md): how a version is cut
