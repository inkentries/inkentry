---
license: apache-2.0
base_model: codefuse-ai/F2LLM-v2-330M
tags:
  - gguf
  - quantized
  - embeddings
  - sentence-similarity
library_name: gguf
---

# F2LLM-v2-330M Q8_0 GGUF

Q8_0-quantized GGUF build of [`codefuse-ai/F2LLM-v2-330M`](https://huggingface.co/codefuse-ai/F2LLM-v2-330M),
distributed for use as the bundled embedding model in
[Spelunk](https://github.com/spelunk-cloud/spelunk).

## What this is

A single GGUF file containing the F2LLM-v2-330M weights quantized from the
original BF16 safetensors to **Q8_0**:

- Projection matmul weights and the token-embedding table → **Q8_0**
- RMSNorm weights → **F32** (kept full precision; negligible size)

This is the same artifact `spelunk-server` would otherwise produce on-device on
first run; distributing it pre-quantized cuts the first-run download from
~638 MB to ~339 MB and keeps steady-state disk use at ~339 MB.

| File | Approx. size | sha256 |
|------|--------------|--------|
| `f2llm-v2-330m-q8_0.gguf` | ~339 MB | `2c12aad2951f1d9a3b457f890a2586d1ee19b755b377c0fb424e856e615b8f2b` |

## Provenance

- **Base model:** `codefuse-ai/F2LLM-v2-330M`
- **Pinned source revision:** `1239cdd544b24c247ed75df2ae22e5a401ac4659`
- **Quantization:** Q8_0 from the original BF16 weights (see above)
- **Architecture / dim:** Qwen3 decoder, 896-dim embeddings

## License & attribution

Licensed under the **Apache License, Version 2.0**, inherited from the base
model. The full license text is in [`LICENSE`](./LICENSE) and the modification
notice is in [`NOTICE`](./NOTICE).

This is a **modified** copy of `codefuse-ai/F2LLM-v2-330M`: the weights have been
quantized to Q8_0 from the original BF16 weights. No other modifications were
made.

## Usage in Spelunk

`spelunk-server` downloads this GGUF directly when the
`SPELUNK_EMBEDDER_GGUF_REPO` config points at this repository. Otherwise it falls
back to downloading the upstream BF16 safetensors and quantizing on device.
