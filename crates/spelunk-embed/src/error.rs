use thiserror::Error;

/// Errors raised by the native candle embedding engine
/// ([`crate::embedder_native`]). Kept distinct from a bare `anyhow::Error` so
/// a caller (e.g. `spelunk-server`'s HTTP handlers) can match on the failure
/// kind instead of string-matching a message.
#[derive(Error, Debug)]
pub enum EmbedError {
    #[error("tokenization failed: {0}")]
    Tokenization(String),

    #[error("inference failed: {0}")]
    Inference(String),
}
