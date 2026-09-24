// Commands that hand-write ANSI color codes print through `cprintln!`, so the
// on/off decision lives only in `color_enabled`.

use std::io::IsTerminal as _;
use std::sync::OnceLock;

/// `--color` flag value.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorChoice {
    /// Color on when stdout is a terminal and `NO_COLOR` is unset (default).
    #[default]
    Auto,
    /// Always emit color, regardless of tty state or `NO_COLOR`.
    Always,
    /// Never emit color.
    Never,
}

static CHOICE: OnceLock<ColorChoice> = OnceLock::new();

// Must be set before any command prints output.
pub(crate) fn set_color_choice(choice: ColorChoice) {
    let _ = CHOICE.set(choice);
}

// Pure so it is testable without a tty or process env. `NO_COLOR` counts only
// when non-empty (no-color.org).
pub(crate) fn resolve_color(
    choice: ColorChoice,
    no_color_env: Option<&str>,
    stdout_is_terminal: bool,
) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            let no_color = no_color_env.is_some_and(|v| !v.is_empty());
            !no_color && stdout_is_terminal
        }
    }
}

pub(crate) fn color_enabled() -> bool {
    resolve_color(
        CHOICE.get().copied().unwrap_or_default(),
        std::env::var("NO_COLOR").ok().as_deref(),
        std::io::stdout().is_terminal(),
    )
}

macro_rules! cprintln {
    () => { println!() };
    ($($arg:tt)*) => {{
        let line = format!($($arg)*);
        if $crate::cli::cmd::color::color_enabled() {
            println!("{line}");
        } else {
            println!("{}", inkentry_core::utils::strip_ansi(&line));
        }
    }};
}
pub(crate) use cprintln;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_is_off_when_stdout_is_not_a_terminal() {
        assert!(!resolve_color(ColorChoice::Auto, None, false));
    }

    #[test]
    fn auto_is_on_when_stdout_is_a_terminal_and_no_color_unset() {
        assert!(resolve_color(ColorChoice::Auto, None, true));
    }

    #[test]
    fn no_color_wins_even_on_a_terminal() {
        assert!(!resolve_color(ColorChoice::Auto, Some("1"), true));
    }

    #[test]
    fn empty_no_color_does_not_disable_color() {
        assert!(resolve_color(ColorChoice::Auto, Some(""), true));
    }

    #[test]
    fn explicit_always_overrides_no_color_and_non_tty() {
        assert!(resolve_color(ColorChoice::Always, Some("1"), false));
    }

    #[test]
    fn explicit_never_overrides_a_terminal() {
        assert!(!resolve_color(ColorChoice::Never, None, true));
    }
}
