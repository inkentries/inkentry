//! Native F2LLM-v2-330M embedder for spelunk.
//!
//! This crate owns the candle-based embedding engine (Qwen3 decoder,
//! 896-dim, Q8_0-quantized GGUF, Metal/GPU on macOS). It is a library so both
//! the bundled `spelunk-server` binary and downstream consumers that need a
//! local embedder can depend on it directly.
//!
//! Two load entry points are provided:
//!
//! * [`NativeEmbedder::load_from_hub`] downloads (and, first run, quantizes) the
//!   model through the Hugging Face Hub cache.
//! * [`NativeEmbedder::load_from_path`] loads the model from local files already
//!   on disk with **zero network access** — for callers that fetch the GGUF,
//!   tokenizer, and config themselves.
//!
//! Both produce the same [`NativeEmbedder`], which implements
//! [`spelunk_core::embeddings::EmbeddingBackend`]. The whole engine is gated
//! behind the `embed-native` cargo feature (on by default); add the `metal`
//! feature for Metal GPU acceleration on macOS.

#[cfg(feature = "embed-native")]
mod embedder_native;

#[cfg(feature = "embed-native")]
pub use embedder_native::{DIM, NativeEmbedder};
