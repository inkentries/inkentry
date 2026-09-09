# Model attribution

inkentry-server bundles an embedding model rather than calling an external
embedding endpoint. `cargo-about` (see `about.toml`) covers Rust dependency
licenses, but it does not cover the model weights downloaded at runtime, so they
are attributed here.

Looking for how to configure an external LLM or embedding endpoint instead?
See [Third-party models](third-party-models.md).

## F2LLM-v2-330M (embedder)

- **Model:** `codefuse-ai/F2LLM-v2-330M`
- **Upstream:** https://huggingface.co/codefuse-ai/F2LLM-v2-330M
- **Pinned source revision:** `1239cdd544b24c247ed75df2ae22e5a401ac4659`, the
  provenance anchor for the weights, tokenizer, and config redistributed in
  `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF` (see below). Not used at runtime;
  update it (and regenerate/re-upload the artifacts) if the pin ever moves.
- **License:** Apache License 2.0 (declared via the upstream Hugging Face
  model-card license tag). Full text:
  https://www.apache.org/licenses/LICENSE-2.0
- **Use in inkentry:** loaded by `inkentry-server` as the 896-dim semantic
  embedding backend (Qwen3 decoder architecture, llama.cpp engine).

### Modification notice (Apache-2.0 §4)

inkentry redistributes a **modified** copy of these weights: the original BF16
safetensors are **quantized to Q8_0** (projection matmuls and the token-embedding
table are stored Q8_0; RMSNorm weights are kept F32) and packaged as a single
GGUF file. No other changes are made to the weights.

inkentry fetches this pre-quantized Q8_0 GGUF from a Hugging Face repository it
owns (`spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF`); that artifact carries its own
`LICENSE`, `NOTICE`, and model card reproducing this attribution. The canonical
llama.cpp GGUF embeds its own tokenizer and config, so it is the only file the
engine needs — nothing else is fetched. **The GGUF is not fetched from the
third-party upstream repo at runtime** (it comes from our own first-party repo).
Set `INKENTRY_EMBEDDER_GGUF_REPO` to a different repo to fetch it from there
instead (it must host that file). See `docs/embedder-artifact/` for the text
that accompanies the distributed artifact.

### Why our repo sits under `spelunk-cloud`

`spelunk-cloud` is the Hugging Face org of inkentry's predecessor product, and
the embedder repo stays there deliberately. Moving it under an inkentry-named
org would buy a tidier URL and nothing else, and it would not be free: the org
name is part of the `hf-hub` cache key, and the air-gapped provisioning
procedure in `docs/server-setup.md` hard-codes the current cache directory name
in a copy-paste command. Hosting the artifacts ourselves instead would trade a
working third-party dependency for an ongoing hosting obligation. Neither is worth
buying, so the name is settled rather than unfinished rebranding: that org is
ours, and it is the repo to fetch from. The default is `DEFAULT_GGUF_REPO` in
`crates/inkentry-server/src/embed_hub.rs`.

### Other bundled inference dependencies

The llama.cpp engine (`llama-cpp-2` / `llama-cpp-sys-2`) and the
Hugging Face hub client (`hf-hub`) are Rust crates and are
covered by `cargo-about` / `about.toml`; they are not re-listed here.
