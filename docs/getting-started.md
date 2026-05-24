# Getting Started

## 1. Install spelunk

Download the latest binary for your platform from the [releases page](https://github.com/spelunk-cloud/spelunk/releases) and put it somewhere on your `$PATH`:

```bash
# macOS (Apple Silicon)
curl -L https://github.com/spelunk-cloud/spelunk/releases/latest/download/spelunk-v0.8.0-aarch64-apple-darwin.tar.gz \
  | tar -xz && chmod +x spelunk spelunk-server && sudo mv spelunk spelunk-server /usr/local/bin/

# macOS (Intel)
curl -L https://github.com/spelunk-cloud/spelunk/releases/latest/download/spelunk-v0.8.0-x86_64-apple-darwin.tar.gz \
  | tar -xz && chmod +x spelunk spelunk-server && sudo mv spelunk spelunk-server /usr/local/bin/

# macOS (universal)
curl -L https://github.com/spelunk-cloud/spelunk/releases/latest/download/spelunk-v0.8.0-universal-apple-darwin.tar.gz \
  | tar -xz && chmod +x spelunk spelunk-server && sudo mv spelunk spelunk-server /usr/local/bin/

# Linux x86_64
curl -L https://github.com/spelunk-cloud/spelunk/releases/latest/download/spelunk-v0.8.0-x86_64-unknown-linux-gnu.tar.gz \
  | tar -xz && chmod +x spelunk spelunk-server && sudo mv spelunk spelunk-server /usr/local/bin/

# Linux ARM64
curl -L https://github.com/spelunk-cloud/spelunk/releases/latest/download/spelunk-v0.8.0-aarch64-unknown-linux-gnu.tar.gz \
  | tar -xz && chmod +x spelunk spelunk-server && sudo mv spelunk spelunk-server /usr/local/bin/

# Verify
spelunk --version
```

Building from source? See [Building](building.md).

## 2. Start using it immediately — no setup required

No configuration needed. From inside any git repository, you can immediately:

```bash
# Trace callers and callees for any symbol
spelunk graph validate_token

# Full-text search
spelunk search "error handling" --mode text

# Store a decision for your team
spelunk memory add --kind decision \
  --title "Chose token bucket for rate limiting" \
  --body "Simpler than sliding window; sufficient for <1k RPS"

# List your decisions
spelunk memory list --kind decision
```

Memory is stored in git notes — no server, no database, no configuration.

## 3. Start an agent session

When your agent or team is starting a new coding session, pull all relevant context in one command:

```bash
# Agent entry point — pulls decisions, requirements, questions, handoffs
spelunk context

# Filter by kind
spelunk context --kind decision

# Get JSON for machine processing
AGENT=true spelunk context
```

## 4. Set up automatic memory harvesting (optional)

Install a git post-commit hook so `spelunk` automatically extracts memories from commit messages:

```bash
spelunk hooks install
```

Other developers without `spelunk` installed are unaffected. To remove:

```bash
spelunk hooks uninstall
```

---

## Optional: Semantic Search

For concept-level search (finding code by meaning rather than text), you need:
1. An OpenAI-compatible embedding server (e.g., LM Studio, Ollama)
2. To index your project

### Set up an embedding server

Choose one:

- **[LM Studio](https://lmstudio.ai/)** — desktop app; load a model and enable the local server (default port `1234`)
- **[Ollama](https://ollama.com/)** — `ollama serve` (default port `11434`)
- **vLLM / OpenAI-compatible proxy** — point `api_base_url` at your endpoint

**Recommended embedding model:** `google/embeddinggemma-300m-qat` (300M params, low VRAM, fast)

### Configure spelunk (optional)

If you want to use a non-default server, create `~/.config/spelunk/config.toml`:

```toml
# ~/.config/spelunk/config.toml

# Default: http://127.0.0.1:1234 (LM Studio)
api_base_url = "http://127.0.0.1:1234"

# Must match your server's model identifier
embedding_model = "text-embedding-embeddinggemma-300m-qat"

# Embedding batch size (tune if you run out of memory)
batch_size = 32
```

### Index your project for semantic search

Once your embedding server is running:

```bash
cd /path/to/your/project
spelunk init
```

This:
1. Registers your project in the global registry
2. Parses every source file and indexes chunks
3. Embeds chunks using your configured server
4. Stores everything in `~/.local/share/spelunk/<project-slug>.db`

Output:
```
spelunk initialised for my-project

  Index:   142 files, 1 840 chunks
  DB:      ~/.local/share/spelunk/my-project.db
  Embeddings: 1 840 vectors
```

**Subsequent runs** only re-index changed files (via blake3 hash):

```bash
spelunk index /path/to/your/project
```

Force a full re-index after changing the embedding model:

```bash
spelunk index /path/to/your/project --force
```

### Use semantic search

```bash
# Finds code by concept, not just text
spelunk search "error handling in the HTTP layer"

# Hybrid search (semantic + full-text)
spelunk search "authentication" --mode hybrid

# Expand with 1-hop call graph
spelunk search "authentication" --graph

# Fit results within a token budget for agents
spelunk search "database layer" --budget 4000

# Machine-readable output
spelunk search "database migrations" --format json
```

### Check index health

```bash
spelunk status                              # index statistics
spelunk check                               # verify index is up to date
spelunk check --porcelain --files           # list files that need re-indexing
```

---

## Next steps

- [Commands reference](commands.md) — every flag and option
- [Memory](memory.md) — storing project context across sessions
- [Agent Guide](agent-guide.md) — using `spelunk` with AI coding agents
- [Building from source](building.md) — for contributors and platform builders

---

## Team setup: Shared memory with spelunk-server

Working with a team? Run `spelunk-server` so everyone shares decisions, requirements, and context instead of siloing them locally.

Each team member's code stays local — only memory travels to the server.

### Quick setup

Add `.spelunk/config.toml` at your repo root (commit it):

```toml
# .spelunk/config.toml — commit this, no secrets
memory_server_url = "http://spelunk.internal:7777"
project_id        = "my-awesome-app"
```

Each developer adds the API key to their personal config:

```toml
# ~/.config/spelunk/config.toml — never commit this
memory_server_key = "your-shared-api-key"
```

After setup, all `spelunk memory` commands transparently use the server. To migrate existing local memories:

```bash
spelunk memory push
```

For full setup and deployment guide: **[Server setup](server.md)** — Docker, configuration, API reference.
