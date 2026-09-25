// Lifecycle lines for the detached continuation children. Off a TTY indicatif
// hides its bar, so without these an empty log looks like a worker that never
// started. Their stderr is the log file: plain text, no colour or cursor movement.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::IndexArgs;

// Off outside the continuation children, so the embed loop can report
// unconditionally without changing foreground output.
static ACTIVE: AtomicBool = AtomicBool::new(false);

// A slow embedder runs small batches for hours; a line per batch would flood a
// log meant to be read by eye.
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Embed,
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

pub(super) fn stamp(msg: &str) -> String {
    format!(
        "[{}] {msg}",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )
}

pub(super) fn emit(msg: impl AsRef<str>) {
    if is_active() {
        eprintln!("{}", stamp(msg.as_ref()));
    }
}

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
