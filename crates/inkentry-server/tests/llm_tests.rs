// Consolidated LLM-routing test binary: groups the previously separate
// credential, daemon-e2e, key-precedence, and transport-guard test files
// into one integration test crate to cut per-binary link overhead.

#[path = "llm_tests/llm_credential_invariants.rs"]
mod llm_credential_invariants;
#[path = "llm_tests/llm_daemon_e2e.rs"]
mod llm_daemon_e2e;
#[path = "llm_tests/llm_key_precedence_e2e.rs"]
mod llm_key_precedence_e2e;
#[path = "llm_tests/llm_key_transport_guard.rs"]
mod llm_key_transport_guard;
#[path = "llm_tests/removed_embedding_flags_subprocess.rs"]
mod removed_embedding_flags_subprocess;
