// Consolidated search test binary: groups the previously separate memory-read and unified-search-surface test files into one integration test crate to cut per-binary link overhead.

mod plumbing_helpers;

#[path = "search_tests/read_memory.rs"]
mod read_memory;
#[path = "search_tests/unified_search_surface.rs"]
mod unified_search_surface;
