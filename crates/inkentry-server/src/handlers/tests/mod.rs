// Most themes drive the router via `tower::ServiceExt::oneshot`; timeout and
// concurrency bind a real TCP listener instead.

mod support;

mod batch_dedupe_tests;
mod batch_tests;
mod concurrency_tests;
mod embed_tests;
mod health_tests;
mod liveness_tests;
mod llm_tests;
mod notes_tests;
mod repair_tests;
mod search_tests;
mod sync_tests;
mod timeout_tests;
mod wire_shape_tests;
