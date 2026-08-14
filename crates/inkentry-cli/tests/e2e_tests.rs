// Consolidated small-e2e test binary: groups the previously separate ADR/color-output/import/compose test files into one integration test crate to cut per-binary link overhead.

mod plumbing_helpers;

#[path = "e2e_tests/adr037_p2_auto_start_scope.rs"]
mod adr037_p2_auto_start_scope;
#[path = "e2e_tests/color_output.rs"]
mod color_output;
#[path = "e2e_tests/import_dump.rs"]
mod import_dump;
#[path = "e2e_tests/import_travels_with_repo.rs"]
mod import_travels_with_repo;
#[path = "e2e_tests/integration_compose.rs"]
mod integration_compose;
