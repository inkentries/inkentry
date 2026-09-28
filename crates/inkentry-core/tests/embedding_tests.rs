// One embedding test binary, not several: groups the unit and property
// tests to cut per-binary link overhead.

#[path = "embedding_tests/prop_embeddings.rs"]
mod prop_embeddings;
#[path = "embedding_tests/unit_embeddings.rs"]
mod unit_embeddings;
