// Consolidated server e2e test binary: groups the previously separate
// CLI-sync, health-under-load, HTTP-handler, and TLS-serve test files into
// one integration test crate to cut per-binary link overhead.

mod common;

#[path = "server_e2e_tests/cli_sync_e2e.rs"]
mod cli_sync_e2e;
#[path = "server_e2e_tests/health_under_index_load.rs"]
mod health_under_index_load;
#[path = "server_e2e_tests/integration_server.rs"]
mod integration_server;
#[path = "server_e2e_tests/tls_serve.rs"]
mod tls_serve;
