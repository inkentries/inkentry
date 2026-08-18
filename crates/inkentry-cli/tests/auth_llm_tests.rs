// Consolidated auth/LLM-routing test binary: groups the previously separate auth, credential, and LLM/server-routing test files into one integration test crate to cut per-binary link overhead.

mod plumbing_helpers;

#[path = "auth_llm_tests/auth_config_partial.rs"]
mod auth_config_partial;
#[path = "auth_llm_tests/auth_llm_key.rs"]
mod auth_llm_key;
#[path = "auth_llm_tests/auth_server_keys.rs"]
mod auth_server_keys;
#[path = "auth_llm_tests/backend_kind_diagnostic.rs"]
mod backend_kind_diagnostic;
#[path = "auth_llm_tests/cloud_first_slug_passthrough.rs"]
mod cloud_first_slug_passthrough;
#[path = "auth_llm_tests/command_llm_routing.rs"]
mod command_llm_routing;
#[path = "auth_llm_tests/hooks_install_llm_truthfulness.rs"]
mod hooks_install_llm_truthfulness;
#[path = "auth_llm_tests/inference_server_message.rs"]
mod inference_server_message;
#[path = "auth_llm_tests/llm_daemon_spawn_e2e.rs"]
mod llm_daemon_spawn_e2e;
#[path = "auth_llm_tests/logout_credential_scope.rs"]
mod logout_credential_scope;
#[path = "auth_llm_tests/server_key_resolution_e2e.rs"]
mod server_key_resolution_e2e;
#[path = "auth_llm_tests/tls_trust.rs"]
mod tls_trust;
