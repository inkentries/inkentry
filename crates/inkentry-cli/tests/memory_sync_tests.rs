// Consolidated memory sync/replication test binary: groups the previously separate push-sync, relates-to-edge, and team-memory test files into one integration test crate to cut per-binary link overhead.

mod plumbing_helpers;

#[path = "memory_sync_tests/absent_memory_store.rs"]
mod absent_memory_store;
#[path = "memory_sync_tests/memory_push_sync_partial_failure.rs"]
mod memory_push_sync_partial_failure;
#[path = "memory_sync_tests/memory_push_sync_total_failure.rs"]
mod memory_push_sync_total_failure;
#[path = "memory_sync_tests/memory_read_mode_notice.rs"]
mod memory_read_mode_notice;
#[path = "memory_sync_tests/memory_relates_to_edge.rs"]
mod memory_relates_to_edge;
#[path = "memory_sync_tests/memory_relates_to_edge_sync.rs"]
mod memory_relates_to_edge_sync;
#[path = "memory_sync_tests/plumbing_store_resolution.rs"]
mod plumbing_store_resolution;
#[path = "memory_sync_tests/team_memory_read_path.rs"]
mod team_memory_read_path;
#[path = "memory_sync_tests/team_memory_real_server_e2e.rs"]
mod team_memory_real_server_e2e;
