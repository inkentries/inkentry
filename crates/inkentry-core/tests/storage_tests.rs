// Consolidated storage/persistence test binary: groups the previously
// separate DB, git-notes, worktree, and convention test files into one
// integration test crate to cut per-binary link overhead.

mod common;

#[path = "storage_tests/conventions.rs"]
mod conventions;
#[path = "storage_tests/escape_like_integration.rs"]
mod escape_like_integration;
#[path = "storage_tests/integration_db.rs"]
mod integration_db;
#[path = "storage_tests/integration_git_notes.rs"]
mod integration_git_notes;
#[path = "storage_tests/unit_graph.rs"]
mod unit_graph;
#[path = "storage_tests/worktree_index_resolution.rs"]
mod worktree_index_resolution;
