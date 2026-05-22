# Getting Started

## 1. Install spelunk

Download the latest binary for your platform from the [releases page](https://github.com/spelunk-cloud/spelunk/releases) and put it somewhere on your `$PATH`:

```bash
# macOS (Apple Silicon) — universal binary also available
curl -L https://github.com/spelunk-cloud/spelunk/releases/latest/download/spelunk-v0.7.0-aarch64-apple-darwin.tar.gz \
  | tar -xz && chmod +x spelunk spelunk-server && sudo mv spelunk spelunk-server /usr/local/bin/

# macOS (Intel)
curl -L https://github.com/spelunk-cloud/spelunk/releases/latest/download/spelunk-v0.7.0-x86_64-apple-darwin.tar.gz \
  | tar -xz && chmod +x spelunk spelunk-server && sudo mv spelunk spelunk-server /usr/local/bin/

# macOS (universal — works on both Intel and Apple Silicon)
curl -L https://github.com/spelunk-cloud/spelunk/releases/latest/download/spelunk-v0.7.0-universal-apple-darwin.tar.gz \
  | tar -xz && chmod +x spelunk spelunk-server && sudo mv spelunk spelunk-server /usr/local/bin/

# Linux x86_64
curl -L https://github.com/spelunk-cloud/spelunk/releases/latest/download/spelunk-v0.7.0-x86_64-unknown-linux-gnu.tar.gz \
  | tar -xz && chmod +x spelunk spelunk-server && sudo mv spelunk spelunk-server /usr/local/bin/

# Linux ARM64
curl -L https://github.com/spelunk-cloud/spelunk/releases/latest/download/spelunk-v0.7.0-aarch64-unknown-linux-gnu.tar.gz \
  | tar -xz && chmod +x spelunk spelunk-server && sudo mv spelunk spelunk-server /usr/local/bin/

# Verify
spelunk --version
```

> Replace `v0.1.0` with the version you want. The URL pattern is:
> `https://github.com/spelunk-cloud/spelunk/releases/latest/download/spelunk-<version>-<target>.tar.gz`

> Building from source? See [Building](building.md).

## 2. Start using it

No configuration needed. From inside any git repository:

```bash
# Trace callers and callees for any symbol
spelunk graph validate_token

# Full-text search
spelunk search "error handling" --mode text

# Store a decision
spelunk memory add --kind decision \
  --title "Chose token bucket for rate limiting" \
  --body "Simpler than sliding window; sufficient for <1k RPS"

# Read it back
spelunk memory list --kind decision
```

Memory is stored in git notes — no server, no database, no setup.

## 3. Try search and memory together

```bash
# Search memory for context on a topic
spelunk memory search "why did we choose this"

# Full-text code search
spelunk search "handleRequest" --mode text

# Trace a symbol's call graph
spelunk graph Database --kind calls

# Get JSON output (for agents)
AGENT=true spelunk memory list --kind decision
```

## 4. Set up automatic memory harvesting

Install a git post-commit hook so `spelunk` harvests memory on every commit:

```bash
spelunk hooks install
```

Other developers without `spelunk` installed are unaffected — the hook checks for the binary first.

To remove:

```bash
spelunk hooks uninstall
```

---

## Optional: semantic search

For concept-level search (finding code by meaning rather than text), you need:
1. An OpenAI-compatible embedding server
2. A built index

### Set up an inference server

The easiest options:

- **[LM Studio](https://lmstudio.ai/)** — desktop app for macOS/Windows/Linux; enable the local server (default port `1234`)
- **[Ollama](https://ollama.com/)** — `ollama serve` (default port `11434`)
- **vLLM / any OpenAI proxy** — point `api_base_url` at your endpoint

Recommended models:
- **Embedding** — `google/embeddinggemma-300m-qat` (300M params, low VRAM, fast)
- **Chat (optional)** — any instruction-tuned model; needed only for `memory harvest` and `plan create`

### Configure spelunk

`spelunk` looks for a config file at `~/.config/spelunk/config.toml`:

```toml
# ~/.config/spelunk/config.toml

# LM Studio default:  http://127.0.0.1:1234
# Ollama default:     http://127.0.0.1:11434
api_base_url = "http://127.0.0.1:1234"

# Must match the model's API identifier on your server
embedding_model = "text-embedding-embeddinggemma-300m-qat"

# Optional: enables `memory harvest` and `plan create`
# llm_model = "google/gemma-3n-e4b"

# Embedding batch size — lower if you run out of memory
batch_size = 32

# Default database location (default: ~/.local/share/spelunk/<project-slug>.db)
# db_path = "/custom/path/myproject.db"
```

You can also override the database path per-command with `--db <path>`.

### Index your project

```bash
cd /path/to/your/project
spelunk init
```

`spelunk init`:
1. Registers the project in the global spelunk registry
2. Parses every source file, embeds each chunk, and stores everything in SQLite
3. Prints a summary with file/chunk counts and suggested next commands

```
spelunk initialised for my-project

  Index:   142 files, 1 840 chunks
  DB:      ~/.local/share/spelunk/my-project.db
  Hook:    not installed — run `spelunk hooks install` to add
```

```bash
# Also install the post-commit git hook in one step
spelunk init --hook

# Register without indexing (index later with `spelunk index .`)
spelunk init --no-index
```

Running `spelunk init` again is safe — it won't re-register an existing project.

### Manual indexing

```bash
spelunk index /path/to/your/project

# Force a full re-index (after changing embedding model)
spelunk index /path/to/your/project --force
```

On subsequent runs, only changed files are re-processed (blake3 hash comparison).

### Semantic search

```bash
# Finds code by meaning, not just text
spelunk search "error handling in the HTTP layer"

# Hybrid search (semantic + full-text)
spelunk search "authentication" --mode hybrid

# With call-graph enrichment
spelunk search "authentication" --graph

# Fit results within a token budget
spelunk search "database layer" --budget 4000

# JSON output
spelunk search "database migrations" --format json
```

### Check index health

```bash
spelunk status          # index statistics
spelunk check           # verify index is up to date (exits 1 if stale)
spelunk check --porcelain --files   # list stale files
```

---

## Next steps

- [Commands reference](commands.md) — every flag and option
- [Memory](memory.md) — storing project context across sessions
- [Agent Guide](agent-guide.md) — using `spelunk` with AI coding agents
- [Building from source](building.md) — for contributors and platform builders

## Team setup (shared memory)

Working with teammates? Run `spelunk-server` so the whole team shares memory
instead of each person siloing their own decisions and context.

Add a `.spelunk/config.toml` at your repo root and commit it:

```toml
# .spelunk/config.toml — commit this, it contains no secrets
memory_server_url = "http://spelunk.internal:7777"
project_id        = "my-awesome-app"
```

Each developer adds the API key to their personal config:

```toml
# ~/.config/spelunk/config.toml — never commit
memory_server_key = "shared-team-key"
```

After that, all `spelunk memory` commands transparently use the server. Push
any existing local entries with `spelunk memory push`.

→ **[Server setup guide](server.md)** — Docker, API reference, production tips
