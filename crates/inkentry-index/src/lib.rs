//! The code indexer: language detection and parsing, chunking, graph-edge
//! extraction, PageRank and the structural summaries that feed embedding.
//! What it produces is stored through `inkentry_core::storage`.

pub mod chunker;
#[cfg(feature = "rich-formats")]
pub mod docparser;
pub mod error;
pub mod filter;
pub mod graph;
pub mod mmr;
pub mod pagerank;
pub mod parser;
#[cfg(feature = "rich-formats")]
pub mod pdf;
pub mod summariser;

#[allow(unused_imports)]
pub use chunker::{
    Chunk, ChunkKind, chunk_token_cap, chunker_config_id, set_chunk_token_cap, sliding_window,
};
#[allow(unused_imports)]
pub use parser::SourceParser;
