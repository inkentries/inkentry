// The v1 team-sharing surface trim removes four porcelain memory subcommands
// outright — no hidden aliases, no tombstones. Invoking any of them now yields
// clap's standard unknown-subcommand error (exit 2), and none appear in help.
// The capability they carried lives on elsewhere: one-way transfer moved to
// `inkentry plumbing push` / `inkentry plumbing pull`, and two-way convergence
// stays `inkentry sync`.

mod plumbing_helpers;

use plumbing_helpers::inkentry_bin;

// A removed command is clap's unknown-subcommand error: exit 2, the offending
// name on stderr, nothing on stdout.
fn assert_unknown_subcommand(args: &[&str], name: &str) {
    let out = inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1")
        .args(args)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "`inkentry {}` must be a clap usage error (exit 2); stderr={:?}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "`inkentry {}` must write nothing to stdout; stdout={:?}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unrecognized subcommand") && stderr.contains(name),
        "expected clap's unknown-subcommand error naming `{name}`; stderr={stderr:?}"
    );
}

#[test]
fn memory_push_is_removed() {
    assert_unknown_subcommand(&["memory", "push"], "push");
}

#[test]
fn memory_pull_is_removed() {
    assert_unknown_subcommand(&["memory", "pull"], "pull");
}

#[test]
fn memory_watch_is_removed() {
    assert_unknown_subcommand(&["memory", "watch"], "watch");
}

#[test]
fn memory_since_is_removed() {
    // A trailing timestamp argument is still a removed command, not a valid call.
    assert_unknown_subcommand(&["memory", "since", "123"], "since");
}

// Extract the subcommand names clap lists under `Commands:` in a `--help` dump
// (the first whitespace-delimited token of each indented row).
fn help_subcommands(help: &str) -> Vec<String> {
    let section = match help.split_once("Commands:") {
        Some((_, rest)) => rest,
        None => return Vec::new(),
    };
    let mut names = Vec::new();
    for line in section.lines() {
        if line.trim().is_empty() {
            if names.is_empty() {
                continue;
            }
            break;
        }
        if let Some(name) = line.split_whitespace().next() {
            names.push(name.to_string());
        }
    }
    names
}

#[test]
fn removed_commands_are_absent_from_memory_help() {
    let out = inkentry_bin().args(["memory", "--help"]).output().unwrap();
    let help = String::from_utf8(out.stdout).expect("help is utf-8");
    let names = help_subcommands(&help);

    // Guard against a vacuous parse: the surviving commands must be present.
    assert!(
        names.iter().any(|n| n == "add") && names.iter().any(|n| n == "sync"),
        "expected the surviving memory commands in help; parsed {names:?}"
    );
    for gone in ["push", "pull", "watch", "since"] {
        assert!(
            !names.iter().any(|n| n == gone),
            "`memory {gone}` must be absent from `memory --help`; parsed {names:?}"
        );
    }
}
