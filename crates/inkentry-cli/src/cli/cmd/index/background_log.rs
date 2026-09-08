//! Lifecycle lines for the detached continuation children (`--_embed-phases`,
//! `--_background-phases`), whose stdout and stderr the parent points at
//! `index-background.log`.
//!
//! Detached from a terminal, the child otherwise says nothing until it is
//! done: indicatif hides the progress bar on a non-TTY, and the phase notices
//! only fire after the embed pass. A user sent to the log by `inkentry init`
//! then finds an empty file, which reads the same as a worker that never
//! started. The lines here are what tell the two apart: a start line with the
//! pid, throttled batch progress, and a finish line, or the reason the child
//! stopped early, since that is exactly when someone opens the file.
//!
//! Every line goes to stderr as plain text with a UTC timestamp. Whenever a
//! parent spawned this child that stderr is the log file, on every platform,
//! so nothing here emits colour or cursor movement.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::IndexArgs;

/// Set once by [`activate`] in the continuation modes. Outside them [`emit`] is
/// a no-op, so the embed loop can report progress unconditionally without
/// changing what a foreground run prints.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Least time between two progress lines. A slow embedder runs small batches
/// for a long time, and one line per batch would let a multi-hour run write
/// thousands of near-identical lines into a file meant to be read by eye.
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// Which continuation child this process is, for the lifecycle lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    /// `--_embed-phases`: the embed pass and everything after it.
    Embed,
    /// `--_background-phases`: title-less refinement and conventions only.
    Refinement,
}

impl Phase {
    pub(super) fn of(args: &IndexArgs) -> Option<Self> {
        if args.embed_phases {
            Some(Self::Embed)
        } else if args.background_phases {
            Some(Self::Refinement)
        } else {
            None
        }
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Embed => "background embed",
            Self::Refinement => "background refinement",
        })
    }
}

pub(super) fn activate() {
    ACTIVE.store(true, Ordering::Relaxed);
}

pub(super) fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// `[<utc rfc3339, seconds>] <msg>`.
pub(super) fn stamp(msg: &str) -> String {
    format!(
        "[{}] {msg}",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )
}

/// Write one stamped line to stderr when active; nothing otherwise.
pub(super) fn emit(msg: impl AsRef<str>) {
    if is_active() {
        eprintln!("{}", stamp(msg.as_ref()));
    }
}

/// Decides which committed batches get a progress line: the first, the last,
/// and otherwise at most one per [`PROGRESS_MIN_INTERVAL`].
pub(super) struct ProgressThrottle {
    last_emit: Option<Instant>,
}

impl ProgressThrottle {
    pub(super) fn new() -> Self {
        Self { last_emit: None }
    }

    pub(super) fn due(&mut self, is_last: bool) -> bool {
        self.due_at(Instant::now(), is_last)
    }

    fn due_at(&mut self, now: Instant, is_last: bool) -> bool {
        let due = is_last
            || self
                .last_emit
                .is_none_or(|last| now.duration_since(last) >= PROGRESS_MIN_INTERVAL);
        if due {
            self.last_emit = Some(now);
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(clap::Parser, Debug)]
    struct TestCli {
        #[command(flatten)]
        index: IndexArgs,
    }

    fn args(extra: &[&str]) -> IndexArgs {
        let mut argv = vec!["inkentry", "some/path"];
        argv.extend_from_slice(extra);
        TestCli::try_parse_from(argv).expect("parse").index
    }

    #[test]
    fn phase_follows_the_continuation_flag() {
        assert_eq!(Phase::of(&args(&["--_embed-phases"])), Some(Phase::Embed));
        assert_eq!(
            Phase::of(&args(&["--_background-phases"])),
            Some(Phase::Refinement)
        );
        assert_eq!(Phase::of(&args(&[])), None);
        assert_eq!(Phase::of(&args(&["--detach-embed"])), None);
    }

    #[test]
    fn stamped_line_is_utc_seconds_then_message() {
        let line = stamp("background embed started (pid 1)");
        let (ts, rest) = line[1..]
            .split_once("] ")
            .expect("a bracketed timestamp then the message");
        assert_eq!(rest, "background embed started (pid 1)");
        assert_eq!(ts.len(), "2026-01-01T00:00:00Z".len(), "{ts}");
        assert!(
            ts.ends_with('Z'),
            "UTC, so two machines' logs compare: {ts}"
        );
        assert!(!line.contains('\u{1b}'), "no escape codes in a file sink");
    }

    #[test]
    fn progress_lines_are_throttled_but_the_first_and_last_batch_always_land() {
        let mut t = ProgressThrottle::new();
        let start = Instant::now();
        assert!(
            t.due_at(start, false),
            "the first batch says the run is alive"
        );
        assert!(
            !t.due_at(start + Duration::from_secs(1), false),
            "a batch inside the interval is silent"
        );
        assert!(t.due_at(start + PROGRESS_MIN_INTERVAL, false));
        assert!(
            t.due_at(
                start + PROGRESS_MIN_INTERVAL + Duration::from_millis(1),
                true
            ),
            "the final count always lands, whatever the interval"
        );
    }
}
