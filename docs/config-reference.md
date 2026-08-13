# inkentry config reference

Every field in `~/.config/inkentry/config.toml` and `.inkentry/config.toml`,
with defaults, types, and descriptions. Verified against
`crates/inkentry-core/src/config/mod.rs`.

inkentry reads configuration from two TOML files, layered with environment
variable overrides.

## Config files

| File | Scope | Commit to git? |
|------|-------|-----------------|
| `~/.config/inkentry/config.toml` | Personal, machine-wide | No |
| `.inkentry/config.toml` (project root, found by walking up from CWD) | Project-level, team-wide | Yes: contains no secrets by design |

The two files are not interchangeable: most fields are only read from the
personal config, and the project file accepts a deliberately narrow set (see
[Project config fields](#inkentryconfigtoml-project-level) below).

Load order (later overrides earlier):

1. Defaults
2. `~/.config/inkentry/config.toml` (global personal). `server_url` is
   discarded even if present here: a team server is a project-wide choice,
   never a single developer's.
3. `.inkentry/config.toml`, discovered by walking up from the current
   directory (project-level, team-wide). Only `server_url`, `project_id`,
   `server_ca`, and `[index]` are read from this file.
4. Environment variables: `INKENTRY_SERVER_URL`, `INKENTRY_SERVER_KEY`,
   `INKENTRY_PROJECT_ID`, `INKENTRY_SERVER_CA`, `INKENTRY_LLM_URL`,
   `INKENTRY_LLM_MODEL`, `INKENTRY_MODE`.

A variable that is **set** overrides the file even when its value is empty:
`INKENTRY_LLM_URL=""` blanks a configured `llm_url` rather than falling through
to it. Unset the variable to let the file value apply.

Override the global config file path with `-c, --config <path>` on any command
(also settable via `INKENTRY_CONFIG_DIR`, which overrides the whole
`~/.config/inkentry/` directory, not just the file).

---

## Fields (personal config)

These fields are read from `~/.config/inkentry/config.toml`. Unless noted
otherwise, setting them in `.inkentry/config.toml` has no effect: the project
file only accepts the fields listed under
[Project config fields](#inkentryconfigtoml-project-level).

### `db_path`

- **Type:** path
- **Default:** `~/.config/inkentry/index.db`

Path to the SQLite index database file. The project index and memory databases
live alongside it (`index.db`, `memory.db`).

### `llm_model`

- **Type:** string, optional
- **Default:** unset
- **Env override:** `INKENTRY_LLM_MODEL`

Two unrelated jobs, which is worth untangling before you set it.

**Configuration.** Together with `llm_url` below, it is the model name a daemon
the CLI starts sends to that endpoint, passed as the daemon's `--llm-model`. It
is ignored without an `llm_url`: a model with no endpoint is not a
configuration.

**Not a request parameter.** The value is not attached to any inference request
the CLI makes, so it does not choose the model on a server the CLI merely
reaches, such as a team `server_url`; that server's own configuration decides.
`inkentry harvest` does not consult this field at all. Whether a chat
model is actually available depends on the capability tier (a reachable
inference server with a model loaded), independent of this setting.

### `llm_url`

- **Type:** string, optional
- **Default:** unset
- **Env override:** `INKENTRY_LLM_URL`

Base URL of an OpenAI-compatible chat-completions endpoint (a local LM Studio
or Ollama, a self-hosted gateway). When set, the auto-spawned local
`inkentry-server` is started against it and gains LLM capability, which is what
`inkentry harvest` and index-time summaries need.
When unset, the daemon runs without an LLM.

Personal config only. A value in a checked-in `.inkentry/config.toml` is
ignored, like any key outside the project-level allowlist: an endpoint URL is a
per-developer choice, and committing one points the whole team at one machine.

Setting this field is also a statement about where your code may go. Under the
default `local_first` mode (and under `offline`), if `llm_url` is set but the
running local server was not started with it, inkentry stops and asks you to
restart the server rather than falling back to an LLM on `server_url`. Under
`mode = "cloud_first"` that does not apply, because `server_url` is the
inference target there already. See
[Third-party models → The local-LLM guarantee](third-party-models.md#the-local-llm-guarantee-and-where-it-stops).

**Precedence**, highest first: `inkentry server start --llm-url` (for that
daemon only) > `INKENTRY_LLM_URL` > `llm_url` here > unset. An empty value is
handled differently in the two override positions: `INKENTRY_LLM_URL=""`
overrides and blanks this field, leaving no endpoint at all, while
`--llm-url ""` is discarded and leaves the environment or config value in
place. A variable that is set wins even when empty; a flag has to carry a value
to win.

**The credential for this endpoint is never a config field**, here or anywhere
else. Store it with `inkentry auth set-key --llm`, which reads it from stdin or
a prompt and keeps it in the OS secret store, or set `INKENTRY_LLM_KEY` (which
wins over the stored value). Two properties of that are deliberate:

- The CLI reads the credential **only** on the code path that spawns a daemon.
  It is not loaded with the rest of your configuration, so no ordinary command
  authorizes against your keychain for it.
- The spawned daemon **never opens the OS secret store itself.** It is detached,
  and a keychain read from a background process with no user session raises a
  prompt nobody can answer. The CLI resolves the credential in your session and
  passes it to the child out of band, in its environment, never in an argument.

`inkentry-server` refuses to start when a credential resolves and `llm_url` is a
plaintext `http://` URL to a non-loopback host, naming the URL in the error.
The check is scoped to a credential being present, so a keyless LAN endpoint on
`http://192.168.x.x:1234` is unaffected. See
[Third-party models](third-party-models.md#security-properties).

A daemon already running keeps the configuration it was started with. Restart
it with `inkentry server stop && inkentry server start` after changing `llm_url`,
`llm_model`, or the stored credential.

### `llm_context_length`

- **Type:** integer
- **Default:** `8192`

Context-window size (tokens) of the configured LLM, used by `inkentry memory
harvest` (including its `--source claude-code` variant) to split harvest
batches that would overflow the model's window. Set this to match the
context length of the model you have loaded.

### `store_in_git_notes`

- **Type:** boolean
- **Default:** `true`

When true, `inkentry memory add` also appends the new entry as a line of JSON to
`refs/notes/inkentry` on `HEAD`. This keeps memory close to commits, so it travels
with the code. Failure to write the git note is non-fatal: a warning is logged
and the primary SQLite write is unaffected. Set `store_in_git_notes = false` to
opt out.

### `server_url`

- **Type:** string, optional
- **Default:** unset
- **Env override:** `INKENTRY_SERVER_URL`

URL of a team `inkentry-server` instance. When set, memory commands read and write
against that shared server: this is the only configuration that moves memory off
the local machine. A value in the **personal** config is always discarded on
load; set it in `.inkentry/config.toml` (project-level) or via
`INKENTRY_SERVER_URL` instead, since a team server is a shared, project-wide
choice.

`server_url` must be `https://` unless it points at loopback (`127.0.0.1`,
`::1`, or `localhost`). A non-loopback `http://` URL is rejected at startup,
with no opt-out, because the CLI attaches a bearer token to these requests.

An auto-discovered loopback `inkentry-server` is used for inference only and is
never a memory store; it does not require this field to be set. See the
[server setup guide](server-setup.md) for putting TLS in front of a deployed
team server.

### `mode`

- **Type:** string, optional (`offline` / `local_first` / `cloud_first`)
- **Default:** unset (derived from `server_url`; see below)
- **Env override:** `INKENTRY_MODE`

Controls where memory reads and writes go, and whether the CLI ever contacts a
configured `server_url`.

| mode | reads | writes | cloud contact |
|------|-------|--------|----------------|
| `offline` | local | local | never, even if `server_url` is set |
| `local_first` | local | local, then async background sync | best-effort |
| `cloud_first` | server (error if unreachable) | server (error if unreachable) | required |

When unset, the effective mode is derived: no `server_url` means `offline`; a
configured `server_url` means `local_first`. `INKENTRY_NO_SERVER=1` forces
`offline` regardless of this setting, as a hard kill-switch. `mode` is only
read from the personal config, not from `.inkentry/config.toml`. See
[Team server and sync modes](memory.md#team-server-and-sync-modes) for the
full picture.

`mode` also decides which server answers LLM calls for
`inkentry harvest` and index-time summaries, and it is the one setting
that changes whether a configured [`llm_url`](#llm_url) keeps your code off a
remote LLM. See
[Third-party models → How inkentry finds an LLM](third-party-models.md#how-inkentry-finds-an-llm).

### `server_key`

- **Type:** string, optional
- **Default:** unset
- **Env override:** `INKENTRY_SERVER_KEY`

This field only resolves the **cloud-kind** bearer, used for inkentry cloud
requests (`INKENTRY_SERVER_KEY` if set, otherwise the `[auth].access_token`
written by `inkentry login`). It is **not** the effective credential for a
self-hosted team `inkentry-server`: since the per-origin key scoping in
ADR-071, that bearer is resolved separately, keyed by the server's origin, so
a developer holding keys for two different self-hosted servers doesn't have
them collide or leak into each other.

A bare `server_key` left in your personal `~/.config/inkentry/config.toml` is
migrated into your OS keychain (macOS Keychain, Linux Secret Service, Windows
Credential Manager) and stripped from the file automatically the next time it
loads; it is then migrated a second time, into the per-origin key store, the
first time it's needed to authenticate a specific server.

Prefer one of these instead of hand-editing this field:

- `inkentry auth set-key --server <url>` stores a per-server key directly in the
  secret store (the key is read from stdin or an interactive prompt, never a
  flag, so it never lands in shell history or `ps` output).
- `inkentry auth list-servers` lists which server origins have a stored key
  (never prints key material).
- `INKENTRY_SERVER_KEY` works everywhere, including CI, and always takes
  precedence over both the per-origin store and `[auth]`.

Do **not** commit a `server_key` to `.inkentry/config.toml`: the project file
does not accept this field at all (see
[Project config fields](#inkentryconfigtoml-project-level)); a line present
there anyway is silently dropped and never resolves to a credential. See
[`inkentry auth`](commands.md#inkentry-auth) for the full command reference.

### `project_id`

- **Type:** string, optional
- **Default:** unset (derived at runtime if absent)
- **Env override:** `INKENTRY_PROJECT_ID`

Human-readable project slug used to route memory on a team `inkentry-server`. It
is sent to the server exactly as configured, whether it is a slug or a UUID:
there is no lookup and nothing is cached. Required when `server_url` points at
a non-loopback address (or provide it once via `inkentry sync --project <slug>`).
If `server_url` is a loopback address, `project_id` may be omitted: inkentry
derives a stable id from the project's git remote, or from a hash of the local
path if there is no remote. Normally set in `.inkentry/config.toml` alongside
`server_url`.

### `server_ca`

- **Type:** path, optional
- **Default:** unset
- **Env override:** `INKENTRY_SERVER_CA`

Path to a PEM CA bundle to trust in addition to the built-in roots, for a team
`server_url` presenting a certificate signed by a self-signed or internal CA.
Verification stays on: this only adds a trust anchor, it does not disable
checks. Valid in either config file. See
[trusting the server's certificate](server-setup.md#trusting-the-servers-certificate-on-the-client)
for the full walkthrough.

### `[auth]`

- **Type:** table, optional
- **Default:** absent
- **Managed by:** `inkentry login`, `inkentry org switch` - do not hand-edit

WorkOS device-flow tokens for inkentry cloud, written by `inkentry login` under
the global config's `[auth]` table:

```toml
[auth]
access_token = "..."
refresh_token = "..."
expires_at = 1234567890
org_id = "org_..."
```

While `access_token` is unexpired, it is the source of the `Authorization:
Bearer` token every inkentry cloud request sends; it does not apply to a
self-hosted `server_url`, which resolves its own credential separately (see
`server_key` above). `refresh_token` rotates an expired access token and backs
organization switching. The file is written with `0600` permissions. This
table is not read from `.inkentry/config.toml`.

Every field is optional: a partial table (for example a login without an org,
which omits `org_id`, or a hand-trimmed file) is tolerated and never blocks
commands that need no credentials. A missing or empty `access_token` is read as
"not logged in" (no bearer is sent); a missing `expires_at` is treated as
expired (forcing a refresh); a missing `org_id` applies no organization
scoping.

### `[index]`

- **Type:** table
- **Default:** `use_default_excludes = true`, `detect_generated = true`, `exclude = []`

Controls the built-in index-time file filter that skips generated, vendored,
and machine-data files. Distinct from the unconditional sensitive-file
exclusion (`.env`, key files), which is not configurable.

```toml
[index]
exclude = ["vendor/**", "!vendor/README.md"]
use_default_excludes = true
detect_generated = true
```

- **`exclude`** - extra gitignore-syntax lines layered on top of the built-in
  defaults. A `!pattern` line re-includes a path the defaults would otherwise
  drop (last match wins). Cannot re-include a sensitive file.
- **`use_default_excludes`** - whether to apply the built-in default exclude
  set at all.
- **`detect_generated`** - whether to skip files whose header self-declares as
  generated (`@generated`, or `// Code generated ... DO NOT EDIT.`).

Also valid in `.inkentry/config.toml`, where it overrides the personal value
per field: an absent key in the project table leaves the personal (or default)
value in place.

---

## `.inkentry/config.toml` (project-level)

Safe to commit; contains no secrets by design. Only four keys are read from
this file - `server_url`, `project_id`, `server_ca`, and `[index]` - anything
else (including any personal field documented above) is silently ignored.

```toml
# .inkentry/config.toml
server_url = "https://inkentry.internal.example.com"
project_id = "my-awesome-app"
server_ca = "/etc/inkentry/internal-ca.pem"

[index]
exclude = ["fixtures/**"]
```

**`server_key` is deliberately not accepted here.** A credential in a
committed file stays in the repo's history forever and is readable by anyone
with repo access, so the project config has no field for it at all: a stray
`server_key` line is silently dropped, and the file's other keys still load
normally. Use `inkentry auth set-key --server <url>` (or `INKENTRY_SERVER_KEY`
in CI) to set a shared team credential per developer instead.

**`llm_url` is not accepted here either**, for a related reason: it is the
endpoint a credential is presented to, and it is a per-developer choice. A
committed value would point every teammate's local daemon at whichever machine
the author happened to be running an LLM on. Set it in the personal config or
via `INKENTRY_LLM_URL`.

## `~/.config/inkentry/config.toml` (personal)

```toml
# ~/.config/inkentry/config.toml

# Chat-completions endpoint the local inkentry-server is started against, and
# the model it is asked for. Store the endpoint's credential with
# `inkentry auth set-key --llm`, never here.
llm_url = "http://127.0.0.1:1234"
llm_model = "google/gemma-3n-e4b"
llm_context_length = 8192

# Keep memory close to commits (default)
store_in_git_notes = true
```

Written for you by `inkentry login` (the `[auth]` table) and by the one-time
`server_key` migration; you don't normally hand-edit either.

---

## What you cannot configure

**The embedding model.** It is pinned product-wide —
`codefuse-ai/F2LLM-v2-330M`, 896 dimensions — and computed only by the bundled
native embedder in `inkentry-server`. There is no key for choosing it and no
relocation option.

**`inference_url`.** Populated at runtime when inkentry auto-discovers a
loopback server, and never read from either TOML file.

An unrecognised key parses without error and does nothing, so a typo is silent.
Check spelling against the field list above if a setting appears to have no
effect.

---

## Environment variable overrides

| Variable | Overrides / effect |
|----------|--------------------|
| `INKENTRY_SERVER_URL` | `server_url` |
| `INKENTRY_SERVER_KEY` | `server_key` (takes precedence over the per-origin secret store and `inkentry login` tokens) |
| `INKENTRY_PROJECT_ID` | `project_id` |
| `INKENTRY_SERVER_CA` | `server_ca` |
| `INKENTRY_LLM_URL` | `llm_url` |
| `INKENTRY_LLM_MODEL` | `llm_model` |
| `INKENTRY_LLM_KEY` | Credential for the `llm_url` endpoint (takes precedence over the secret-store entry written by `inkentry auth set-key --llm`). Not a `config.toml` field. |
| `INKENTRY_MODE` | `mode` (`offline` / `local_first` / `cloud_first`; an unrecognized value is a hard error) |
| `INKENTRY_NO_SERVER=1` | Kill-switch: forces `offline` mode and disables server autostart, regardless of `mode` or `server_url` |
| `INKENTRY_CLOUD_URL` | inkentry cloud API URL used by `login` / `org` (default `https://api.inkentry.com`) |
| `INKENTRY_SECRET_STORE` | Secret-store backend: `auto` (default), `keychain`, or `file` |
| `AGENT=true` | Forces JSON output for commands that support it (not a config field) |

This table covers only the env vars that override a `config.toml` field.
`commands.md`'s [Environment variables](commands.md#environment-variables)
section lists the complete set, including `INKENTRY_CONFIG_DIR`,
`INKENTRY_STATE_DIR`, `RUST_LOG`, and `EDITOR`/`VISUAL`, which don't map onto a
field here.

---

## What's next

- [Stability contract](stability.md) - which of these keys semver freezes, which file each may be set in, and the deprecation policy for removing one
- [Server setup](server-setup.md) - `server_url` / `server_key` in a team deployment
- [Project memory](memory.md) - `store_in_git_notes` and memory backends
- [Commands reference](commands.md) - `-c, --config` and per-command overrides
