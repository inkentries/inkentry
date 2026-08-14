// Consolidated embedding test binary: groups the previously separate
// embedding unit and property tests into one integration test crate to
// cut per-binary link overhead.

#[path = "embedding_tests/prop_embeddings.rs"]
mod prop_embeddings;
#[path = "embedding_tests/unit_embeddings.rs"]
mod unit_embeddings;
