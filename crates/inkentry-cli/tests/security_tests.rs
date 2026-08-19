// Consolidated security test binary: groups the previously separate egress, secret-scanning, and hook-path test files into one integration test crate to cut per-binary link overhead.

mod plumbing_helpers;

#[path = "security_tests/crash_safety.rs"]
mod crash_safety;
#[path = "security_tests/daemon_spawn_call_sites.rs"]
mod daemon_spawn_call_sites;
#[path = "security_tests/egress_containment.rs"]
mod egress_containment;
#[path = "security_tests/egress_trap.rs"]
mod egress_trap;
#[path = "security_tests/fail_closed_no_project.rs"]
mod fail_closed_no_project;
#[path = "security_tests/hooks_path_resolution.rs"]
mod hooks_path_resolution;
#[path = "security_tests/loopback_discovery_warnings.rs"]
mod loopback_discovery_warnings;
#[path = "security_tests/loopback_isolation.rs"]
mod loopback_isolation;
#[path = "security_tests/secret_scanner.rs"]
mod secret_scanner;
