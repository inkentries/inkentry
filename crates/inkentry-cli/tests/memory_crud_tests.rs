// Consolidated memory CRUD test binary: groups the previously separate memory add/list/dedupe/reindex/removed-command test files into one integration test crate to cut per-binary link overhead.

mod plumbing_helpers;

#[path = "memory_crud_tests/memory_add_db_scopes_git_notes.rs"]
mod memory_add_db_scopes_git_notes;
#[path = "memory_crud_tests/memory_add_dedupe.rs"]
mod memory_add_dedupe;
#[path = "memory_crud_tests/memory_add_kind_validation.rs"]
mod memory_add_kind_validation;
#[path = "memory_crud_tests/memory_add_secret_gate.rs"]
mod memory_add_secret_gate;
#[path = "memory_crud_tests/memory_dedupe.rs"]
mod memory_dedupe;
#[path = "memory_crud_tests/memory_list_format.rs"]
mod memory_list_format;
#[path = "memory_crud_tests/memory_list_source_ref.rs"]
mod memory_list_source_ref;
#[path = "memory_crud_tests/memory_reindex.rs"]
mod memory_reindex;
#[path = "memory_crud_tests/memory_removed_commands.rs"]
mod memory_removed_commands;
