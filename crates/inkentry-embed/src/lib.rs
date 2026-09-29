//! F2LLM-v2-330M embedder for inkentry.
//!
//! This crate owns the llama.cpp-based embedding engine (F2LLM-v2-330M, 896-dim,
//! canonical llama.cpp GGUF; GPU via Metal on macOS and Vulkan on Windows/Linux,
//! CPU everywhere else). It is a library so both the bundled `inkentry-server`
//! binary and downstream consumers that need a local embedder can depend on it.
//!
//! The [`LlamaEmbedder`] implements the crate's own [`EmbeddingBackend`] trait
//! (re-exported by inkentry-core at `inkentry_core::embeddings::EmbeddingBackend`).
//! The engine lives behind the default-on `llama` feature: a consumer that only
//! needs the trait + [`MODEL_ID`] (inkentry-core, so inkentry-cli doesn't
//! statically link an embedder it only ever calls over HTTP) depends on this
//! crate with `default-features = false`. Add `llama-metal` or `llama-vulkan`
//! for GPU acceleration; `inkentry-server` resolves the GGUF via its own Hugging
//! Face Hub acquisition path (`embed_hub` module) and constructs the embedder.

mod backend;
pub use backend::{EmbedLane, EmbeddingBackend};

/// Stable provenance id for the native embedding model, `<repo-shortname>@<dim>`.
/// An exact-match token: never parse it. A requantization or hardware-portability
/// rebuild of the same model must not change this — only a genuine model swap
/// (different weights or vector space) does, which forces a re-index.
///
/// Changing this value requires memory.db embedding-provenance stamping first:
/// an unstamped `note_embeddings` vector is assumed to be this model, which
/// only holds while this is the sole model ever shipped.
pub const MODEL_ID: &str = "F2LLM-v2-330M@896";

/// Embedding dimension of the sole shipped model (F2LLM-v2-330M, 896-dim).
pub const DIM: usize = 896;

#[cfg(feature = "llama")]
mod embedder_llama;
#[cfg(feature = "llama")]
pub use embedder_llama::{DEFAULT_EMBED_POOL_SIZE, DeviceRequest, LlamaEmbedder};

#[cfg(feature = "llama")]
mod error;
#[cfg(feature = "llama")]
pub use error::EmbedError;

#[cfg(feature = "llama")]
mod vector;
