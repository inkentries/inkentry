# Third-party models

`inkentry-server` bundles a native embedding model, so semantic search works
with no external endpoint and no way to relocate it: the embedding model and
its compute path are both pinned product-wide. LLM-backed features are
different: the server has no LLM of its own, and proxies those calls to an
external OpenAI-compatible chat-completions endpoint that you configure. This
page covers wiring that up.

Looking for the bundled embedder's license and provenance instead? See
[Model attribution](model-attribution.md).

## Configuring an external LLM endpoint

The CLI never talks to an LLM directly, only through `inkentry-server`. So the
endpoint is always the *server's* configuration. How you set it depends on
which server you mean.

### If you use the auto-managed local daemon

Configure it on the client. The CLI resolves the endpoint and hands it to every
daemon it starts, so you set it once rather than per launch.

| Setting | Personal `config.toml` | Environment | `inkentry server start` flag |
|---|---|---|---|
| Endpoint | `llm_url` | `INKENTRY_LLM_URL` | `--llm-url` |
| Model | `llm_model` | `INKENTRY_LLM_MODEL` | `--llm-model` |
| Credential | not a config key, by design | `INKENTRY_LLM_KEY` | no flag, by design |

`llm_url` and `llm_model` are read from the **personal** config
(`~/.config/inkentry/config.toml`) only. A value in a checked-in
`.inkentry/config.toml` is ignored: an endpoint URL is a per-developer choice,
and committing one points the whole team at one machine. See
[Config reference](config-reference.md#llm_url).

The credential is not a config key at all. Store it once with:

```bash
inkentry auth set-key --llm
```

It is read from stdin if piped, otherwise from an interactive prompt, and kept
in your OS secret store (macOS Keychain, Linux Secret Service, Windows
Credential Manager, or an owner-only `secrets.toml` when no keychain is
available). It is never accepted as a flag value or a positional argument. Set
`INKENTRY_LLM_KEY` instead when you need a non-interactive path such as CI.

**Precedence**, highest first:

| Value | Order |
|---|---|
| Endpoint | `inkentry server start --llm-url` (that daemon only) > `INKENTRY_LLM_URL` > `llm_url` in the personal config > unset |
| Model | `inkentry server start --llm-model` > `INKENTRY_LLM_MODEL` > `llm_model` in the personal config > unset |
| Credential | `INKENTRY_LLM_KEY` > the secret-store entry written by `inkentry auth set-key --llm` > unset |

A model with no endpoint is not a configuration: if nothing resolves an
endpoint, the daemon starts without an LLM whatever the model is set to.

An **empty** value is handled differently in the two override positions. They
answer the same-shaped question in opposite directions, so it is worth knowing
which is which before it surprises you:

- `INKENTRY_LLM_URL=""` counts as an override like any other, so it **blanks**
  the personal config's `llm_url` rather than falling through to it. The net
  result is no endpoint, which makes an exported empty value a way to switch
  the configured endpoint off for a shell. `INKENTRY_LLM_MODEL=""` does the same
  to `llm_model`.
- `--llm-url ""` does the opposite: a blank flag value is not treated as an
  instruction, so it is **discarded** and the environment or config value still
  applies. `--llm-model ""` behaves the same way.

The short rule: an environment variable that is *set* always wins, even when
empty; a flag has to carry a value to win.

The off-switch works because the CLI sets all three LLM variables on the daemon
it starts, rather than letting the daemon inherit them. `inkentry-server` reads
`INKENTRY_LLM_URL` and `INKENTRY_LLM_MODEL` itself, so a value the CLI resolved
away would otherwise still reach it: an exported empty endpoint would arrive as
a configured-but-empty one. Whatever the CLI resolves is what the daemon sees,
and nothing else is.

Both halves are covered by tests, including through a real spawn, so neither
will change by accident. They are described here as current behaviour rather
than promised by the [stability contract](stability.md#config), because the two
positions answer the same question in opposite directions and reconciling them
is still open. Pass a value you mean, and the question does not arise.

### If you run `inkentry-server` yourself

Pass flags to the binary (verified against `inkentry-server --help`, v0.9.5):

| Flag | Env | Purpose |
|---|---|---|
| `--llm-url` | `INKENTRY_LLM_URL` | Base URL of an OpenAI-compatible chat-completions server (e.g. LM Studio, Ollama, vLLM). |
| `--llm-model` | `INKENTRY_LLM_MODEL` | Model name to send to that endpoint (e.g. `google/gemma-3n-e4b`). |
| `--llm-reasoning-effort` | `INKENTRY_LLM_REASONING_EFFORT` | `reasoning_effort` sent on every request. Defaults to `none`, so reasoning models answer instead of spending the token budget on chain-of-thought. Use `minimal`/`low`/`medium`/`high` to allow reasoning, or `default` to omit the field for endpoints that reject it. |
| `--llm-key` | | Credential for that endpoint, passed inline. Visible in the process table, so prefer the alternatives. |
| `--llm-key-file` | | File whose whole trimmed contents are the credential. An unreadable path is fatal, never a fall-through to another source. |

Neither key flag is bound to an environment variable, deliberately. The
credential can still come from `INKENTRY_LLM_KEY`, but it enters at its own rank
rather than through a flag: precedence is `--llm-key` > `--llm-key-file` >
`INKENTRY_LLM_KEY` > unset. Binding the variable to `--llm-key` would let merely
exporting it silently outrank a `--llm-key-file` you also passed.

`--llm-key` is the endpoint's credential, and is a different secret from
`--key`, which is this server's own inbound bearer.

A blank value from any source, on either side, reads as unset rather than as an
empty credential.

### Security properties

These are design constraints, not incidental behaviour, so they are stated
rather than left to be inferred:

- **The credential never travels in an argument from the CLI.** No input to
  `inkentry` causes `--llm-key` or `--llm-key-file` to be emitted into the
  spawned daemon's argv, because argv is world-readable through the process
  table. The endpoint URL and model do travel as arguments: neither is secret,
  and `ps` showing which endpoint a daemon serves is a diagnostic feature.
- **The spawned daemon never reads your OS secret store.** It is detached and
  long-lived, and on macOS a keychain read from a background process with no
  user session raises an authorization prompt that nobody can see or answer.
  The CLI resolves the credential in your own session and passes it to the
  child out of band, in its environment. Nothing else in the daemon reaches for
  a secret store.
- **Resolving the credential costs nothing on other commands.** It is not a
  config field and is not read when configuration loads; only the code path
  that spawns a daemon reads it. An ordinary `inkentry search` does not
  authorize against your keychain for it.
- **A credential is not sent over plaintext to another host.** If a credential
  resolves and the endpoint is a plaintext `http://` URL to anything other than
  loopback, `inkentry-server` refuses to start:

  ```
  Error: invalid server URL "http://192.168.1.10:1234": plaintext http:// is only
  allowed to a loopback address (127.0.0.1/::1/localhost); use https:// for any
  other host. An LLM key is configured, so "http://192.168.1.10:1234" would send
  it in the clear: use an https:// endpoint, or unset the key
  ```

  The check applies **only when a credential is present**, so an existing
  keyless LAN endpoint (LM Studio or Ollama on `http://192.168.x.x:1234`) keeps
  working exactly as before. If you hit this refusal, the endpoint is one you
  are authenticating to over an unencrypted network hop: switch it to `https://`
  or drop the credential.
- **The credential is never printed.** Not by `inkentry auth set-key --llm`, not
  in the server's logs at any level, and not in the refusal message above.
- **A keyless endpoint stays keyless.** With no credential resolved the
  upstream request carries no `Authorization` header at all, byte-identical to
  previous releases, so a local endpoint that rejects unexpected headers is
  unaffected.

### What this unlocks

- **`inkentry harvest`**: LLM-based decision extraction. All three sources
  need an LLM: `--source git` (commits), `--source claude-code` (agent session
  history), and `--source failures`.

**That is the whole list.** `harvest` is the only feature in inkentry that
reaches for an LLM. In particular `inkentry index` does not: its chunk summaries
are composed deterministically from the parse, with no model, no key and no
network. Everything on this page is about `harvest` alone.

### How inkentry finds an LLM

LLM inference and embedding are routed **separately**, and the two can end up on
different servers in the same command. Nothing on this page changes where your
code is embedded: embedding keeps its own rule, in which the default
`local_first` mode always prefers the local embedder even when `server_url` is
set. What follows is the LLM rule only.

In order:

1. **Offline mode** (`INKENTRY_NO_SERVER=1`, or `mode = "offline"`): there is no
   LLM, and nothing is probed.
2. **Your local server serves an LLM**: it is used.
3. **`llm_url` is set but your local server does not serve an LLM**: inkentry
   stops and asks you to restart the server. It does **not** fall through to
   `server_url`. See [The local-LLM guarantee](#the-local-llm-guarantee-and-where-it-stops).
4. **A configured `server_url` serves an LLM**: it is used.
5. **Otherwise**: no LLM is available.

Availability at steps 2 and 4 is decided by what the reachable server reports in
its `/v1/health` capabilities, not by what your config file says. A setting
cannot tell you whether the running daemon ever picked it up, which is the whole
point of step 3.

A server built before LLM support existed advertises a legacy capability without
being able to answer LLM calls. That case is detected — availability is keyed on
`llm.complete` alone — and treated as "no LLM here", so an older team server does
not turn into a broken route.

### Why `harvest` could not run

When routing finds no LLM, `inkentry harvest` **fails** rather than degrading:
it cannot do its job without one. It prints one of three messages, each opening
`'inkentry harvest' cannot run: `, naming the cause and the next step. They are
ordinary output rather than log lines, so you do not need `RUST_LOG` to see them.

**No LLM anywhere** (rule 5):

```
'inkentry harvest' cannot run: no LLM is available.
There are two ways to get one:
  set `llm_url` in ~/.config/inkentry/config.toml to your own chat-completions endpoint, then run `inkentry server stop` and `inkentry server start`;
  or set `server_url` to a inkentry server that already provides one.
```

**A local LLM is configured, but the running server does not serve it** (rule 3).
This is the stale-daemon case: you set `llm_url` after the daemon was already
running.

```
'inkentry harvest' cannot run: your local inkentry server is running without the LLM endpoint you set in `llm_url`, so it cannot answer LLM requests.
A running server keeps the settings it started with, so restart it to pick yours up:
  inkentry server stop
  inkentry server start
```

**Offline mode** (rule 1) has a message, but in practice you will not reach it:
`harvest` stops earlier on the embedding requirement, with the pre-existing
`requires inkentry-server` error.

```
'inkentry harvest' cannot run: offline mode is on, so no inference will run.
Turn offline mode off to enable it: unset INKENTRY_NO_SERVER, or remove `mode = "offline"` from your inkentry config.
```

### The local-LLM guarantee, and where it stops

**In `local_first` (the default) and in `offline`: if you have set `llm_url`,
your code is never sent to a remote LLM.** If the local server is not serving
that endpoint, inkentry stops and tells you to restart it rather than quietly
using `server_url` instead. That is deliberate, not an accident of ordering: you
asked for a local LLM, and substituting a remote one would be a privacy
surprise, not a graceful fallback. The message for that case never mentions
`server_url`, so it cannot nudge you toward the very thing you did not choose.

**In `cloud_first` this guarantee does not apply.** There the configured
`server_url` *is* your inference target, so it is matched at rule 2 and rule 3
never runs: LLM calls go to that server even with `llm_url` set. This is
consistent rather than contradictory, because `cloud_first` already routes
embedding to the same server, so your chunk text reaches it either way. But it
is a real boundary. If you want the guarantee, stay on `local_first` (or
`offline`); `mode` is read from your personal config or `INKENTRY_MODE`, never
from a checked-in `.inkentry/config.toml`.

### If the server itself has no LLM

The server's own `/llm/complete` route returns `503` with
`"This server has no LLM configured. Set INKENTRY_LLM_URL and INKENTRY_LLM_MODEL."`

### Loopback (local dev) setup

Set the endpoint once in your personal config:

```toml
# ~/.config/inkentry/config.toml
llm_url = "http://127.0.0.1:1234"   # your LM Studio / Ollama / vLLM endpoint
llm_model = "your-chat-model-id"
```

If the endpoint needs a credential, store it now:

```bash
inkentry auth set-key --llm     # reads from stdin or a prompt, never argv
```

Then restart the auto-managed daemon, because **a daemon already running keeps
the configuration it was started with**:

```bash
inkentry server stop      # if one is already running
inkentry server start     # picks up the configuration above
```

A change to `llm_url`, `llm_model`, or the stored credential does not reach a
running daemon. Nothing restarts it for you: killing a daemon as a side effect
of an unrelated command is worse than a stale configuration, so the restart is
yours to make.

Environment variables work as well, and take precedence over the config file:

```bash
export INKENTRY_LLM_URL="http://127.0.0.1:1234"
export INKENTRY_LLM_MODEL="your-chat-model-id"
export INKENTRY_LLM_KEY="<your-endpoint-credential>"   # only if the endpoint is keyed

inkentry server stop
inkentry server start
```

Or override them for a single daemon without changing either:

```bash
inkentry server start --llm-url http://127.0.0.1:1234 --llm-model your-chat-model-id
```

`inkentry harvest` and index-time summaries now
work against the auto-discovered loopback server, no `config.toml` change
needed: they fill in the loopback URL for you when no explicit `server_url` is
set.

### Team server setup

Pass the same two flags when you start the deployed `inkentry-server` (see
[Server setup](server-setup.md)):

```bash
inkentry-server --host 0.0.0.0 --port 7777 \
  --tls-cert /etc/inkentry/tls-cert --tls-key /etc/inkentry/tls-key \
  --llm-url https://llm-host:1234 --llm-model your-chat-model-id \
  --llm-key-file /etc/inkentry/llm-key
```

Drop `--llm-key-file` if the endpoint takes no credential, in which case
`--llm-url http://llm-host:1234` is accepted too. With a credential, a
plaintext `http://` endpoint on any host but loopback is refused at startup
(see [Security properties](#security-properties)).

Every client already sets an explicit `server_url` to reach a team server, so
`harvest` and index-time summaries are all unlocked with no
extra client-side configuration.

## Native embedder artifact source

Embeddings have no external endpoint or config: the model is pinned
product-wide to F2LLM-v2-330M at 896 dimensions, computed only by the bundled
native embedder. `INKENTRY_EMBEDDER_GGUF_REPO` points that *bundled native*
embedder at an alternate source for the same F2LLM-v2-330M GGUF and tokenizer
artifacts, not a different model. See [Model attribution](model-attribution.md).

## Running with no network access at all

`INKENTRY_EMBEDDER_GGUF_REPO` above still calls out over the network, just to a
different (self-hosted) source. For a host with no route out at all, not
even to an alternate source, `--model-dir` / `INKENTRY_MODEL_DIR` loads the
bundled native embedder from a directory you provision ahead of time instead
of fetching it from Hugging Face Hub. See [Server setup → Air-gapped /
no-egress install](server-setup.md#air-gapped--no-egress-install) for the
directory layout and the fetch-and-transfer procedure.

## Related

- [Server setup](server-setup.md): deploying `inkentry-server`, TLS, client configuration
- [Model attribution](model-attribution.md): license and provenance for the bundled embedder
- [Getting started](getting-started.md): the zero-setup local path
