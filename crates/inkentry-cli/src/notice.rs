// Notices go to stderr through `enotice!` so `search --quiet` is honoured in one
// place, including in capability probes that never see the flag. Errors and the
// multi-user server warning deliberately bypass it: `--quiet` must never hide
// either.

use std::sync::OnceLock;

static QUIET: OnceLock<bool> = OnceLock::new();

pub(crate) fn set_quiet(quiet: bool) {
    let _ = QUIET.set(quiet);
}

// Defaults to printing so a path reached before `main` records the choice stays loud.
pub(crate) fn notices_enabled() -> bool {
    !QUIET.get().copied().unwrap_or(false)
}

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

    // `QUIET` is set once per process, so only the default state is testable here.
    #[test]
    fn notices_print_when_no_choice_was_recorded() {
        assert!(notices_enabled());
    }
}
