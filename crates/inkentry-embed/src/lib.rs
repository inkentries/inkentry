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
pub use backend::EmbeddingBackend;

/// Stable provenance id for the native embedding model, `<repo-shortname>@<dim>`.
/// Exact-match token: never parse it. Requantization or a hardware-portability
/// rebuild of the same model must NOT change this — only a genuine model swap
/// (different weights / vector space) does, which forces a re-index.
///
/// Before changing this value, ship memory.db embedding-provenance stamping
/// first: unstamped `note_embeddings` vectors are assumed to be this model
/// (backfill rule "unstamped ⇒ F2LLM-v2-330M@896"), an invariant that only
/// holds while this is the sole model ever shipped. See the 2026-07-26
/// `requirement` entry in inkentry memory and task inkentry-oss^286 for the
/// acceptance criteria that work must meet.
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
