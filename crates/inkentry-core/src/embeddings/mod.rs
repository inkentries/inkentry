/// Trait every embedding backend must implement. Owned by `inkentry-embed` (so
/// that crate stays storage-free); re-exported here at the historical path.
pub use inkentry_embed::EmbeddingBackend;

/// Stable provenance id for the native embedding model. Single source of truth
/// is `inkentry_embed::MODEL_ID`; re-exported here so server and CLI share it.
pub use inkentry_embed::MODEL_ID;

/// The embedding vector dimension produced by the default native model (F2LLM-v2-330M, 896-dim).
pub const EMBEDDING_DIM: usize = 896;

/// Precision tag carried alongside a client-pushed memory vector. Memory
/// vectors cross to a shared server, so they are ALWAYS full-precision fp32,
/// never the int8 quantisation used for the local code index. The accept side
/// rejects any other precision, so nothing else is ever sent.
pub const PUSHED_VECTOR_PRECISION: &str = "fp32";

/// The `vector_model` tag a vector-accepting server validates for exact string
/// equality: the model *family* portion of [`MODEL_ID`] (`"F2LLM-v2-330M@896"`)
/// with the `@<dim>` suffix stripped. The dimension travels as the vector's own
/// length, checked separately, so carrying it in the tag as well would make two
/// peers whose model constants differ only in suffix formatting disagree about
/// an identical vector.
///
/// Both sides of the contract call this, so the push side and the accept side
/// cannot drift into two spellings of the same model.
pub fn pushed_vector_model_tag() -> &'static str {
    MODEL_ID.split('@').next().unwrap_or(MODEL_ID)
}

/// Serialise a float vector to raw little-endian f32 bytes for a sqlite-vec
/// `float[N]` column. Used for the full-precision memory-note vector table
/// (`note_embeddings`, `FLOAT[896]`). See `docs/architecture.md` ("Why two
/// vector-storage formats").
pub fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Quantise an L2-normalised float vector to `int8` bytes for a sqlite-vec
/// `int8[N]` column. F2LLM vectors are unit vectors, so each component maps to
/// `round(x * 127)` clamped to `[-127, 127]` — 4× smaller than f32, and since
/// the scaling is uniform L2 ranking is preserved (callers rescale by
/// `INT8_SCALE`). Used only for the chunk/snapshot tables (`embeddings`,
/// `snapshot_embeddings`). See `docs/architecture.md` ("Why two vector-storage
/// formats").
pub fn vec_to_int8_blob(v: &[f32]) -> Vec<u8> {
    v.iter()
        .map(|&x| ((x * 127.0).round().clamp(-127.0, 127.0) as i8) as u8)
        .collect()
}

/// Factor by which a sqlite-vec `int8` L2 distance exceeds the equivalent f32
/// distance, given the `* 127` quantisation in [`vec_to_int8_blob`]. Divide raw
/// int8 distances by this to keep them on the same scale as the old f32 index.
pub const INT8_SCALE: f32 = 127.0;

/// Deserialise raw little-endian bytes back to a float vector.
pub fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|&chunk| f32::from_le_bytes(chunk))
        .collect()
}

/// Dequantise an `int8[N]` blob (as stored in the `embeddings` table by
/// [`vec_to_int8_blob`]) back to a float vector, rescaled by `1/127`. Cosine
/// similarity is scale-invariant, so this is exact enough to reuse a stored
/// primary vector as an MMR centroid without a fresh embed.
pub fn int8_blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.iter().map(|&x| (x as i8) as f32 / INT8_SCALE).collect()
}
