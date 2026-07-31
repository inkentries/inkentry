# Third-party models

`spelunk-server` bundles a native embedding model, so semantic search works
with no external endpoint and no way to relocate it: the embedding model and
its compute path are both pinned product-wide. LLM-backed features are
different: the server has no LLM of its own, and proxies those calls to an
external OpenAI-compatible chat-completions endpoint that you configure. This
page covers wiring that up.

Looking for the bundled embedder's license and provenance instead? See
[Model attribution](model-attribution.md).

## Configuring an external LLM endpoint

The CLI never talks to an LLM directly, only through `spelunk-server`. So the
endpoint is always the *server's* configuration. How you set it depends on
which server you mean.

### If you use the auto-managed local daemon

Configure it on the client. The CLI resolves the endpoint and hands it to every
daemon it starts, so you set it once rather than per launch.

| Setting | Personal `config.toml` | Environment | `spelunk server start` flag |
|---|---|---|---|
| Endpoint | `llm_url` | `SPELUNK_LLM_URL` | `--llm-url` |
| Model | `llm_model` | `SPELUNK_LLM_MODEL` | `--llm-model` |
| Credential | not a config key, by design | `SPELUNK_LLM_KEY` | no flag, by design |

`llm_url` and `llm_model` are read from the **personal** config
(`~/.config/spelunk/config.toml`) only. A value in a checked-in
`.spelunk/config.toml` is ignored: an endpoint URL is a per-developer choice,
and committing one points the whole team at one machine. See
[Config reference](config-reference.md#llm_url).

The credential is not a config key at all. Store it once with:

```bash
spelunk auth set-key --llm
```

It is read from stdin if piped, otherwise from an interactive prompt, and kept
in your OS secret store (macOS Keychain, Linux Secret Service, Windows
Credential Manager, or an owner-only `secrets.toml` when no keychain is
available). It is never accepted as a flag value or a positional argument. Set
`SPELUNK_LLM_KEY` instead when you need a non-interactive path such as CI.

**Precedence**, highest first:

| Value | Order |
|---|---|
| Endpoint | `spelunk server start --llm-url` (that daemon only) > `SPELUNK_LLM_URL` > `llm_url` in the personal config > unset |
| Model | `spelunk server start --llm-model` > `SPELUNK_LLM_MODEL` > `llm_model` in the personal config > unset |
| Credential | `SPELUNK_LLM_KEY` > the secret-store entry written by `spelunk auth set-key --llm` > unset |

A model with no endpoint is not a configuration: if nothing resolves an
endpoint, the daemon starts without an LLM whatever the model is set to.

An **empty** value is handled differently in the two override positions. They
answer the same-shaped question in opposite directions, so it is worth knowing
which is which before it surprises you:

- `SPELUNK_LLM_URL=""` counts as an override like any other, so it **blanks**
  the personal config's `llm_url` rather than falling through to it. The net
  result is no endpoint, which makes an exported empty value a way to switch
  the configured endpoint off for a shell. `SPELUNK_LLM_MODEL=""` does the same
  to `llm_model`.
- `--llm-url ""` does the opposite: a blank flag value is not treated as an
  instruction, so it is **discarded** and the environment or config value still
  applies. `--llm-model ""` behaves the same way.

The short rule: an environment variable that is *set* always wins, even when
empty; a flag has to carry a value to win.

The off-switch works because the CLI sets all three LLM variables on the daemon
it starts, rather than letting the daemon inherit them. `spelunk-server` reads
`SPELUNK_LLM_URL` and `SPELUNK_LLM_MODEL` itself, so a value the CLI resolved
away would otherwise still reach it: an exported empty endpoint would arrive as
a configured-but-empty one. Whatever the CLI resolves is what the daemon sees,
and nothing else is.

Both halves are covered by tests, including through a real spawn, so neither
will change by accident. They are described here as current behaviour rather
than promised by the [stability contract](stability.md#config), because the two
positions answer the same question in opposite directions and reconciling them
is still open. Pass a value you mean, and the question does not arise.

### If you run `spelunk-server` yourself

Pass flags to the binary (verified against `spelunk-server --help`, v0.9.5):

| Flag | Env | Purpose |
|---|---|---|
| `--llm-url` | `SPELUNK_LLM_URL` | Base URL of an OpenAI-compatible chat-completions server (e.g. LM Studio, Ollama, vLLM). |
| `--llm-model` | `SPELUNK_LLM_MODEL` | Model name to send to that endpoint (e.g. `google/gemma-3n-e4b`). |
| `--llm-key` | | Credential for that endpoint, passed inline. Visible in the process table, so prefer the alternatives. |
| `--llm-key-file` | | File whose whole trimmed contents are the credential. An unreadable path is fatal, never a fall-through to another source. |

Neither key flag is bound to an environment variable, deliberately. The
credential can still come from `SPELUNK_LLM_KEY`, but it enters at its own rank
rather than through a flag: precedence is `--llm-key` > `--llm-key-file` >
`SPELUNK_LLM_KEY` > unset. Binding the variable to `--llm-key` would let merely
exporting it silently outrank a `--llm-key-file` you also passed.

`--llm-key` is the endpoint's credential, and is a different secret from
`--key`, which is this server's own inbound bearer.

A blank value from any source, on either side, reads as unset rather than as an
empty credential.

### Security properties

These are design constraints, not incidental behaviour, so they are stated
rather than left to be inferred:

- **The credential never travels in an argument from the CLI.** No input to
  `spelunk` causes `--llm-key` or `--llm-key-file` to be emitted into the
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
  that spawns a daemon reads it. An ordinary `spelunk search` does not
  authorize against your keychain for it.
- **A credential is not sent over plaintext to another host.** If a credential
  resolves and the endpoint is a plaintext `http://` URL to anything other than
  loopback, `spelunk-server` refuses to start:

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
- **The credential is never printed.** Not by `spelunk auth set-key --llm`, not
  in the server's logs at any level, and not in the refusal message above.
- **A keyless endpoint stays keyless.** With no credential resolved the
  upstream request carries no `Authorization` header at all, byte-identical to
  previous releases, so a local endpoint that rejects unexpected headers is
  unaffected.

### What this unlocks

- **`spelunk explore`**: the interactive LLM reasoning loop (`/explore`).
- **`spelunk memory harvest`**: LLM-based decision extraction from commits and
  agent sessions.
- **`spelunk index` chunk summaries**: see the caveat below; this one needs an
  explicitly configured `server_url` in addition to the server having an LLM.

### Absence behavior

With no LLM configured on the server:

- `spelunk explore` and `spelunk memory harvest` fail with an actionable error
  naming `server_url` (or, if no server is reachable at all, pointing at
  `spelunk server start`).
- The server's own `/explore` and `/llm/complete` routes return `503` with
  `"This server has no LLM configured. Set SPELUNK_LLM_URL and SPELUNK_LLM_MODEL."`
- `spelunk index` prints `Skipping summaries (no server_url configured)` to
  stderr and continues; a missing LLM never fails an index run.

### Loopback (local dev) setup

Set the endpoint once in your personal config:

```toml
# ~/.config/spelunk/config.toml
llm_url = "http://127.0.0.1:1234"   # your LM Studio / Ollama / vLLM endpoint
llm_model = "your-chat-model-id"
```

If the endpoint needs a credential, store it now:

```bash
spelunk auth set-key --llm     # reads from stdin or a prompt, never argv
```

Then restart the auto-managed daemon, because **a daemon already running keeps
the configuration it was started with**:

```bash
spelunk server stop      # if one is already running
spelunk server start     # picks up the configuration above
```

A change to `llm_url`, `llm_model`, or the stored credential does not reach a
running daemon. Nothing restarts it for you: killing a daemon as a side effect
of an unrelated command is worse than a stale configuration, so the restart is
yours to make.

Environment variables work as well, and take precedence over the config file:

```bash
export SPELUNK_LLM_URL="http://127.0.0.1:1234"
export SPELUNK_LLM_MODEL="your-chat-model-id"
export SPELUNK_LLM_KEY="<your-endpoint-credential>"   # only if the endpoint is keyed

spelunk server stop
spelunk server start
```

Or override them for a single daemon without changing either:

```bash
spelunk server start --llm-url http://127.0.0.1:1234 --llm-model your-chat-model-id
```

`spelunk explore` and `spelunk memory harvest` now work against the
auto-discovered loopback server, no `config.toml` change needed, since both
commands fill in the loopback URL for you when no explicit `server_url` is set.

**Index-time summaries are the exception.** They are gated on an *explicitly
configured* `server_url`, not merely on a reachable server, so they stay off
even against an LLM-configured loopback daemon unless you also set:

```toml
# .spelunk/config.toml
server_url = "http://127.0.0.1:7777"
```

(A loopback `http://` value is allowed here; see
[Server setup → Client configuration](server-setup.md#client-configuration).)

### Team server setup

Pass the same two flags when you start the deployed `spelunk-server` (see
[Server setup](server-setup.md)):

```bash
spelunk-server --host 0.0.0.0 --port 7777 \
  --tls-cert /etc/spelunk/tls-cert --tls-key /etc/spelunk/tls-key \
  --llm-url https://llm-host:1234 --llm-model your-chat-model-id \
  --llm-key-file /etc/spelunk/llm-key
```

Drop `--llm-key-file` if the endpoint takes no credential, in which case
`--llm-url http://llm-host:1234` is accepted too. With a credential, a
plaintext `http://` endpoint on any host but loopback is refused at startup
(see [Security properties](#security-properties)).

Every client already sets an explicit `server_url` to reach a team server, so
`explore`, `memory harvest`, and index-time summaries are all unlocked with no
extra client-side configuration.

## Native embedder artifact source

Embeddings have no external endpoint or config: the model is pinned
product-wide to F2LLM-v2-330M at 896 dimensions, computed only by the bundled
native embedder. `SPELUNK_EMBEDDER_GGUF_REPO` points that *bundled native*
embedder at an alternate source for the same F2LLM-v2-330M GGUF and tokenizer
artifacts, not a different model. See [Model attribution](model-attribution.md).

## Running with no network access at all

`SPELUNK_EMBEDDER_GGUF_REPO` above still calls out over the network, just to a
different (self-hosted) source. For a host with no route out at all, not
even to an alternate source, `--model-dir` / `SPELUNK_MODEL_DIR` loads the
bundled native embedder from a directory you provision ahead of time instead
of fetching it from Hugging Face Hub. See [Server setup → Air-gapped /
no-egress install](server-setup.md#air-gapped--no-egress-install) for the
directory layout and the fetch-and-transfer procedure.

## Related

- [Server setup](server-setup.md): deploying `spelunk-server`, TLS, client configuration
- [Model attribution](model-attribution.md): license and provenance for the bundled embedder
- [Getting started](getting-started.md): the zero-setup local path
