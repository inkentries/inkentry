//! Whether this run prints informational notices, and the one way to print one.
//!
//! A notice is a line about the state of this run that the results do not carry
//! themselves: a warmup caveat, a coverage figure, a stale index, a local server
//! that stopped answering. They go to stderr so stdout stays parseable, and
//! `search --quiet` turns them off for callers who want a clean stderr.
//!
//! The decision lives here rather than at each print site because the sites are
//! spread across the capability probes and the command code, and a site holding
//! its own `if !quiet` has to be given the flag to test. Sites that cannot see
//! the flag were exactly how the first two attempts at this leaked a notice.
//! Print through [`enotice!`] and the decision is taken once, for every notice,
//! including ones added later.
//!
//! Two things deliberately do not come through here:
//!
//! - **Errors.** Anything that ends the run non-zero is not a notice, and
//!   silence would leave the caller with no explanation for the exit code.
//! - **The multi-user server warning** in the capability probe, which says
//!   another user's memory may be exposed. A flag reached for because a shell
//!   renders stderr in red must not be able to hide that.

use std::sync::OnceLock;

static QUIET: OnceLock<bool> = OnceLock::new();

/// Record whether this run suppresses notices. Called once from `main`, before
/// any command runs.
pub(crate) fn set_quiet(quiet: bool) {
    let _ = QUIET.set(quiet);
}

/// Whether informational notices are printed on this run. Defaults to printing,
/// so a code path reached before `main` records the choice stays loud.
pub(crate) fn notices_enabled() -> bool {
    !QUIET.get().copied().unwrap_or(false)
}

/// `eprintln!` for an informational notice: prints unless this run is quiet.
macro_rules! enotice {
    ($($arg:tt)*) => {{
        if $crate::notice::notices_enabled() {
            eprintln!($($arg)*);
        }
    }};
}
pub(crate) use enotice;

#[cfg(test)]
mod tests {
    use super::*;

    // `QUIET` is process-global and set once, so the two states cannot both be
    // exercised in one process. The default is the one worth pinning here: a
    // run that never records a choice still prints, which is what keeps a
    // notice from vanishing on any path that does not go through `main`.
    #[test]
    fn notices_print_when_no_choice_was_recorded() {
        assert!(notices_enabled());
    }
}
