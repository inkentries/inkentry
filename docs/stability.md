# Stability contract

This document says which parts of spelunk you may build on, and what a version
bump is allowed to do to them.

spelunk follows [Semantic Versioning](https://semver.org/). Before 1.0 that
promise is not yet in force: the surfaces below are already treated as stable in
practice, and this document is what they are frozen against when 1.0 ships.

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
  `graph`, `memory`, and every other non-plumbing command. Colours, column
  widths, wording, ordering, and summary lines all change freely. Parsing them
  with `grep`/`awk` will break. Use the plumbing commands, or `search --format
  jsonl`, instead.
- **Log and tracing text.** Message wording, `tracing` targets, span names, and
  log levels. Diagnostics on stderr from any command, including the diagnostics
  that accompany a plumbing exit 2, are advisory text and not a parseable
  interface. The *exit code* is the interface.
- **Internal crate APIs.** `spelunk-core`, `spelunk-cli`, `spelunk-embed`, and
  `spelunk-server` are workspace members, not published to crates.io. Any Rust
  item they expose, `pub` or not, may change in any release. Depending on them
  as a git dependency is unsupported.
- **The `/local/` HTTP routes.** `spelunk-server` registers `/local/relay/push`,
  `/local/relay/poll`, and `/local/relay/ack` outside the documented API. They
  are deliberately absent from `docs/openapi.json` and are internal transport
  between a spelunk client and its own server.
- **Wire-format details of the index.** Embedding vectors, the exact SQL schema
  of any table, and sqlite-vec internals. The *file* is a compatibility surface
  (see On-disk formats below); the SQL inside it is not.

## CLI

**Stable:** command names, subcommand names, flag names (long form), positional
argument order, and exit codes, for every command listed in `spelunk --help`.

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

Porcelain commands use `0`/`1` with their own documented meanings (`check`
exits `1` when the index is stale, for example) and do not follow the plumbing
convention.

## Plumbing JSONL

**Stable:** for every `spelunk plumbing <command>`, the *name and type* of each
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
[Config reference](config-reference.md), across all three config files
(`~/.config/spelunk/config.toml`, `.spelunk/config.toml`, and any file passed to
`--config`).

- Unrecognised keys are ignored rather than rejected. A config written for a
  newer spelunk still loads on an older one, and a config carrying a removed key
  still loads.
- The **project-level allowlist** is itself stable. A checked-in
  `.spelunk/config.toml` is honoured for exactly `server_url`, `project_id`,
  `server_ca`, and `[index]`. Anything else in that file is ignored by design,
  most importantly `server_key`: a repository must never be able to hand a
  secret to whoever clones it. Adding a key to that allowlist is additive and
  allowed; removing one is a breaking change.
- Environment variable overrides (`SPELUNK_*`) are stable on the same terms as
  the keys they override.

### Deprecation policy

Removing or renaming a stable config key follows a fixed sequence:

1. **Alias.** The old key keeps working, mapped onto the new one, for at least
   one full minor release.
2. **Warn.** Using the old key emits a deprecation warning naming the
   replacement.
3. **Remove.** The key is dropped in the next major release, and listed under
   "Removed fields" in [Config reference](config-reference.md) and under
   `### Removed` in the changelog. It then falls back to the
   ignore-unknown-keys rule, so old configs still load; they just stop having
   that effect.

The same three steps apply to CLI flags and to `/v1/` request fields.

#### Worked example: `memory_server_url`

This is the precedent the policy is written from.

1. **Alias.** `server_url` carried `#[serde(alias = "memory_server_url")]`, and
   `server_key` carried `#[serde(alias = "memory_server_key")]`, so an existing
   config kept working untouched. The environment variable
   `SPELUNK_MEMORY_SERVER_URL` was accepted as a fallback for
   `SPELUNK_SERVER_URL`.
2. **Warn.** The environment fallback emitted
   `SPELUNK_MEMORY_SERVER_URL is deprecated; use SPELUNK_SERVER_URL instead`.
3. **Remove.** The aliases and the environment fallback were deleted, the
   changelog recorded the break, and `docs/config-reference.md` gained a
   "Removed fields" row pointing at the replacement. The keys are now unknown
   fields: a config that still carries them loads fine and keeps every other
   field, but the deprecated keys have no effect. Regression tests in
   `crates/spelunk-core/src/config/mod.rs` pin exactly that, so the removal
   cannot silently regress into a partial mapping.

That removal shipped pre-1.0, which is why it landed in a minor release rather
than a major one. After 1.0, step 3 waits for the next major version.

## On-disk formats

The promise here is **forward compatibility of your data**: an upgrade must
never require you to delete a store and rebuild it, and must never lose a
recorded memory. The promise is *not* that the SQL schema stays fixed.

| Store | Versioning | Level |
|---|---|---|
| `.spelunk/index.db` | `PRAGMA user_version`, migrated forward on open | **Stable**: migrations are forward-only and run automatically. The index is derived data, so a rebuild is always a valid recovery. |
| `.spelunk/memory.db` | `PRAGMA user_version`, independent of the index | **Stable**, and stricter: memory is not derived data and cannot be rebuilt. A store from a newer spelunk is refused with an upgrade message rather than opened and damaged. |
| `~/.config/spelunk/registry.db` | none | **Best-effort**. Tables are created idempotently. It holds project registrations, which are re-derivable by re-registering. |
| git notes on `refs/notes/spelunk` | `schema_version` inside each JSON record | **Stable**. A record with a higher `schema_version` than the reader knows is refused rather than misread, and lines that are not spelunk records are left untouched, so the ref can be shared with other tooling. |
| server-side database | sequential migration files | **Internal** to a server deployment, and not a client-facing surface. |

Migrations are **forward-only**. Downgrading spelunk after an upgrade has
migrated a store is not supported.

### `.spelunk/` layout

**Stable:** the directory name `.spelunk/` at the project root, and the names
`config.toml`, `index.db`, and `memory.db` within it. Tooling may rely on
`.spelunk/index.db` marking a project root, which is how spelunk itself
discovers one.

**Internal:** everything else in that directory, including lock files, pid
sidecars, background logs, and the cloud project id cache. Names, formats, and
existence may change.

`~/.config/spelunk/` (config and registry) and `~/.local/state/spelunk/`
(runtime state for the local server) follow the same split: the config file is
stable, the state files are internal.

## Enforcement

A contract nothing checks is a wish. Each promise above is tied to something
that fails CI when it is broken.

| Promise | Enforced by |
|---|---|
| Plumbing JSONL field names and types | `crates/spelunk-cli/tests/golden/plumbing_jsonl_schema.json` plus `crates/spelunk-cli/tests/plumbing_jsonl_contract.rs`. Each command is run for real and its output checked against the committed schema. Required fields must be present and correctly typed; **undeclared fields are accepted**, so additive change passes and removal, rename, or retype fails. |
| Every plumbing command has a declared schema | `golden_schema_covers_every_plumbing_subcommand`, which reads the command list out of clap's own help, so a newly added command cannot ship as an unguarded stable surface. |
| The checker itself actually rejects things | `crates/spelunk-cli/tests/schema_contract_checker.rs`. Without it, a checker that accepted everything would leave every golden file green. It drives removal, rename, and retype across every field of every declared command, and pins the reporting wrapper too, including its refusal of a command that emitted no rows at all. |
| Each declared field is load-bearing, per command | `assert_every_declared_field_is_load_bearing` in `plumbing_jsonl_contract.rs`. Every command's real output is replayed with one declared field dropped, then retyped, and the checker must object each time. Conformance alone would pass against a checker that never rejects anything. |
| Plumbing exit codes 0/1/2 | `crates/spelunk-cli/tests/plumbing_exit_codes.rs`, covering all three codes for every command, including the stdout-is-empty guarantee on exit 2 and the three documented exceptions. |
| `/v1/` matches `docs/openapi.json` | The `openapi-snapshot` job in `.github/workflows/ci.yml`. The spec is generated from the running binary (`cargo run -p spelunk-server -- --print-openapi`) and diffed against the committed file, so a route or schema change that skips regenerating the snapshot fails CI. |
| The above run on every change | `.github/workflows/stability-contract.yml`. |

### Changing a stable surface deliberately

If a change to a stable surface is intended:

1. Confirm it is additive. If it is, the golden schema needs no edit, and the
   tests already pass.
2. If it is not additive, it is a breaking change. It needs a major version, a
   deprecation period first, and a changelog entry under `### Removed` or
   `### Changed`.
3. For the server, regenerate the spec:
   `cargo run -p spelunk-server -- --print-openapi > docs/openapi.json`.
4. Update the golden schema and this document in the same change, so the
   contract and the code never disagree.

## What's next

- [Plumbing and porcelain](plumbing-and-porcelain.md): why the split exists and
  how to script against it
- [Commands](commands.md): the full CLI reference
- [Config reference](config-reference.md): every key, default, and env override
- [Releasing](releasing.md): how a version is cut
