// Consolidated git/harvest test binary: groups the previously separate git-notes, harvest, and links-freshness test files into one integration test crate to cut per-binary link overhead.

mod plumbing_helpers;

#[path = "git_harvest_tests/archive_git_notes_carrier.rs"]
mod archive_git_notes_carrier;
#[path = "git_harvest_tests/edges_git_notes_carrier.rs"]
mod edges_git_notes_carrier;
#[path = "git_harvest_tests/git_notes_fallback.rs"]
mod git_notes_fallback;
#[path = "git_harvest_tests/graph_edges.rs"]
mod graph_edges;
#[path = "git_harvest_tests/harvest_ref_injection.rs"]
mod harvest_ref_injection;
#[path = "git_harvest_tests/harvest_secret_scan.rs"]
mod harvest_secret_scan;
#[path = "git_harvest_tests/harvest_upfront_check.rs"]
mod harvest_upfront_check;
#[path = "git_harvest_tests/init_notes_refspec.rs"]
mod init_notes_refspec;
#[path = "git_harvest_tests/links_freshness.rs"]
mod links_freshness;
