# ADR-002: Server AI Endpoint Contract — Generic Inference Primitives

**Status:** Implemented (v0.8, PR #260)

## Context

`spelunk memory harvest` previously called the local LLM and embedding model
directly from the CLI process (`ActiveLlm` / `ActiveEmbedder`). This required
each developer to run a local inference server (LM Studio or equivalent) and
meant AI model config was duplicated across machines.

## Decision

Introduce two server-side inference primitives:

1. **`POST /v1/projects/{id}/llm/complete`** — raw SSE chat completion.
   The CLI sends messages; the server holds the upstream key and streams tokens
   back. No persistence, no server-added system prompt, no trust assumptions.

2. **`POST /v1/projects/{id}/index/embed`** — already existed. Now also used
   by the CLI for query-time embedding with a `"query:<uuid>"` synthetic chunk_id.

`spelunk memory harvest` routes all LLM calls through `/llm/complete` and all
embedding calls through `/index/embed`. Tier-0 (no `server_url` configured)
emits an actionable error (`harvest_requires_server`).

## Consequences

- **Harvest is Tier 1** (requires `server_url`). Offline use is not supported.
- CLI no longer needs a local LLM or embedding server for harvest.
- The `lm_studio_url` config key is **deprecated** for harvest users; set
  `server_url` instead. See migration note below.
- `GET /v1/health` reports `"llm.complete"` in `capabilities` when an LLM
  backend is configured server-side.

## Migration — `lm_studio_url` users

> **Superseded. Kept as the v0.8 record.** Both halves below have since been
> overtaken: the shipped binary refuses a non-loopback plaintext `http://`
> `server_url` (use `https://`, or a loopback host), and `api_base_url` /
> `lm_studio_url` are now parsed but ignored rather than still serving
> `explore` and `index --summarize`. All inference routes through
> `spelunk-server`. For current `server_url` configuration see
> [Team setup](../getting-started.md#team-setup-shared-memory-with-spelunk-server).

If you previously used `lm_studio_url` (or `api_base_url`) in your config for
`spelunk memory harvest`, update `~/.config/spelunk/config.toml`:

```toml
# Before
api_base_url = "http://127.0.0.1:1234"

# After — point at a spelunk-server instance
server_url = "http://your-spelunk-server:7777"
project_id = "your/project"
server_key  = "..."       # if the server requires auth
```

`api_base_url` / `lm_studio_url` continue to work for `spelunk explore` and
`spelunk index --summarize` (local-inference features), but are no longer used
by harvest.

## Security notes

- The BYOK key never leaves the server. The CLI sends prompts; the server holds
  the upstream provider key (decisions #25/#26).
- Prompt injection is the client's responsibility. `/llm/complete` is a raw
  primitive: the server adds no system prompt and makes no trust assumptions.
- No persistence: messages are request-scoped, never written to the memory DB,
  never logged in plaintext.
