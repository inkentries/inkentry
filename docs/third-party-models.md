# Third-party models

spelunk-server bundles a native embedding model rather than calling an external
embedding endpoint. `cargo-about` (see `about.toml`) covers Rust dependency
licenses, but it does not cover the model weights downloaded at runtime, so they
are attributed here.

## F2LLM-v2-330M (embedder)

- **Model:** `codefuse-ai/F2LLM-v2-330M`
- **Upstream:** https://huggingface.co/codefuse-ai/F2LLM-v2-330M
- **Pinned source revision:** `1239cdd544b24c247ed75df2ae22e5a401ac4659`
  (tracked by the `MODEL_REVISION` constant in
  `crates/spelunk-embed/src/embedder_native.rs`; update both together when the
  pin moves).
- **License:** Apache License 2.0 (declared via the upstream Hugging Face
  model-card license tag). Full text:
  https://www.apache.org/licenses/LICENSE-2.0
- **Use in spelunk:** loaded by `spelunk-server` as the 896-dim semantic
  embedding backend (Qwen3 decoder architecture, candle runtime).

### Modification notice (Apache-2.0 §4)

spelunk redistributes a **modified** copy of these weights: the original BF16
safetensors are **quantized to Q8_0** (projection matmuls and the token-embedding
table are stored Q8_0; RMSNorm weights are kept F32) and packaged as a single
GGUF file. No other changes are made to the weights.

By default spelunk fetches the pre-quantized Q8_0 GGUF from a Hugging Face
repository it owns (`spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF`); that artifact
carries its own `LICENSE`, `NOTICE`, and model card reproducing this attribution.
Set `SPELUNK_EMBEDDER_GGUF_REPO` to a different repo to fetch the pre-quant GGUF
from there, or to `off` to build the GGUF from the upstream BF16 weights on
device (download the original BF16 safetensors from the pinned upstream revision
and quantize locally on first run). See `docs/embedder-artifact/` for the text
that accompanies the distributed artifact.

### Other bundled inference dependencies

The candle runtime (`candle-core`, `candle-nn`, `candle-transformers`), the
Hugging Face hub client (`hf-hub`), and `tokenizers` are Rust crates and are
covered by `cargo-about` / `about.toml`; they are not re-listed here.
