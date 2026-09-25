use anyhow::Result;

use super::IndexArgs;
use crate::capability;

pub(super) fn background_log_path(db_path: &std::path::Path) -> Option<std::path::PathBuf> {
    db_path.parent().map(|d| d.join("index-background.log"))
}

// Not inheriting the parent's streams: a pipe reader (`git commit`, CI) would
// block until the child exits, or SIGPIPE it by closing first.
//
// Appends rather than truncates, since a previous child may still be writing.
// The header is written before the child exists, so a header followed by nothing
// means the child never reached its own start line.
pub(super) fn redirect_to_background_log<'a>(
    cmd: &mut std::process::Command,
    log: Option<&'a std::path::Path>,
) -> Option<&'a std::path::Path> {
    use std::io::Write as _;
    let argv = cmd
        .get_args()
        .map(|a| a.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let opened = log.and_then(|p| {
        let mut out = super::super::helpers::open_log_for_append(p).ok()?;
        let _ = writeln!(
            out,
            "\n=== inkentry {argv} ({}) ===",
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        );
        let err = out.try_clone().ok()?;
        Some((p, out, err))
    });
    match opened {
        Some((path, out, err)) => {
            cmd.stdout(out).stderr(err);
            Some(path)
        }
        // Diagnostics are best-effort and must never fail the index.
        None => {
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            None
        }
    }
}

pub(super) enum EmbedSpawn<'a> {
    Inline,
    Detached {
        log_in_use: Option<&'a std::path::Path>,
        // Lets the caller confirm this child, not a racing process, holds the run lock.
        child_pid: u32,
    },
}

// The child re-parses `IndexArgs`/`Config` from this argv, so anything the parent
// resolved must be forwarded here or it resets to its default. Env and cwd are
// deliberately inherited.
pub(super) fn build_detached_child_command(
    exe: &std::path::Path,
    mode_flag: &str,
    args: &IndexArgs,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("index");
    cmd.arg(&args.path);
    cmd.arg(mode_flag);
    if let Some(db_arg) = &args.db {
        cmd.args(["--db", &db_arg.to_string_lossy()]);
    }
    if let Some(cfg_path) = &args.config_path {
        cmd.args(["--config", &cfg_path.to_string_lossy()]);
    }
    if args.no_summaries {
        cmd.arg("--no-summaries");
    }
    cmd.stdin(std::process::Stdio::null());
    cmd
}

pub(super) fn spawn_embed_subprocess<'a>(
    args: &IndexArgs,
    log: Option<&'a std::path::Path>,
) -> Result<EmbedSpawn<'a>> {
    let mut cmd = build_detached_child_command(&std::env::current_exe()?, "--_embed-phases", args);
    cmd.args(["--batch-size", &args.batch_size.to_string()]);
    let in_use = redirect_to_background_log(&mut cmd, log);
    let _std_handles = super::super::helpers::StdHandlesNotInherited::for_spawn();
    match cmd.spawn() {
        Ok(child) => Ok(EmbedSpawn::Detached {
            log_in_use: in_use,
            child_pid: child.id(),
        }),
        Err(e) => {
            tracing::warn!("failed to spawn detached embed process; embedding inline: {e}");
            Ok(EmbedSpawn::Inline)
        }
    }
}

// A loading embedder still qualifies: the worker owns the readiness wait. The
// terminal states would wait forever.
pub(super) fn detach_embed_eligible(tier: &capability::Tier) -> bool {
    matches!(tier.caps(), Some(c) if c.index_embed)
        || matches!(
            tier.embedder_state(),
            Some(capability::EmbedderState::Loading)
        )
}

// Bounds the release-then-spawn gap a racing `index` can win (normally
// milliseconds) without delaying the common case.
pub(super) const HANDOFF_CONFIRM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
pub(super) const HANDOFF_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(clap::Parser, Debug)]
    struct TestCli {
        #[command(flatten)]
        index: IndexArgs,
    }

    fn sample_index_args() -> IndexArgs {
        TestCli::try_parse_from(["inkentry", "some/path"])
            .expect("parse")
            .index
    }

    #[test]
    fn detached_child_command_inherits_cwd_and_env() {
        let cmd = build_detached_child_command(
            std::path::Path::new("/usr/bin/inkentry"),
            "--_background-phases",
            &sample_index_args(),
        );
        assert!(
            cmd.get_current_dir().is_none(),
            "must inherit the parent's cwd rather than pin one"
        );
        assert!(
            cmd.get_envs().next().is_none(),
            "must inherit the parent's environment rather than clear or override it"
        );
    }

    #[test]
    fn detached_child_command_forwards_config_path_when_resolved() {
        let mut args = sample_index_args();
        args.config_path = Some(std::path::PathBuf::from("/tmp/custom-config.toml"));
        let cmd = build_detached_child_command(
            std::path::Path::new("/usr/bin/inkentry"),
            "--_background-phases",
            &args,
        );
        let argv: Vec<_> = cmd.get_args().collect();
        let pos = argv
            .iter()
            .position(|a| *a == "--config")
            .expect("--config must be forwarded when the parent resolved an override");
        assert_eq!(argv[pos + 1], "/tmp/custom-config.toml");
    }

    #[test]
    fn detached_child_command_omits_config_flag_when_not_resolved() {
        let args = sample_index_args();
        assert!(args.config_path.is_none());
        let cmd = build_detached_child_command(
            std::path::Path::new("/usr/bin/inkentry"),
            "--_background-phases",
            &args,
        );
        let argv: Vec<_> = cmd.get_args().collect();
        assert!(
            !argv.iter().any(|a| *a == "--config"),
            "must not add --config when the parent had no override"
        );
    }

    #[test]
    fn detached_child_command_forwards_no_summaries_to_both_spawn_sites() {
        let mut args = sample_index_args();
        args.no_summaries = true;
        for mode_flag in ["--_background-phases", "--_embed-phases"] {
            let cmd = build_detached_child_command(
                std::path::Path::new("/usr/bin/inkentry"),
                mode_flag,
                &args,
            );
            let argv: Vec<_> = cmd.get_args().collect();
            assert!(
                argv.iter().any(|a| *a == "--no-summaries"),
                "--no-summaries must reach the {mode_flag} child"
            );
        }
    }

    fn redirect_for(mode_flag: &str, log: &std::path::Path) {
        let mut cmd = build_detached_child_command(
            std::path::Path::new("/usr/bin/inkentry"),
            mode_flag,
            &sample_index_args(),
        );
        assert_eq!(redirect_to_background_log(&mut cmd, Some(log)), Some(log));
    }

    #[test]
    fn background_log_header_names_the_argv_and_spawn_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("index-background.log");
        redirect_for("--_embed-phases", &log);

        let content = std::fs::read_to_string(&log).expect("header written at open");
        let header = header_lines(&content).next().expect("one header line");
        assert!(
            header.starts_with("=== inkentry index some/path --_embed-phases ("),
            "{header}"
        );
        assert!(
            header.ends_with("Z) ==="),
            "UTC timestamp closes it: {header}"
        );
    }

    #[test]
    fn a_second_spawn_appends_rather_than_truncating_the_earlier_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("index-background.log");
        redirect_for("--_embed-phases", &log);
        redirect_for("--_background-phases", &log);

        let content = std::fs::read_to_string(&log).expect("log readable");
        let headers: Vec<&str> = header_lines(&content).collect();
        assert_eq!(headers.len(), 2, "both spawns kept: {content}");
        assert!(headers[0].contains("--_embed-phases"), "{content}");
        assert!(headers[1].contains("--_background-phases"), "{content}");
    }

    fn header_lines(content: &str) -> impl Iterator<Item = &str> {
        content.lines().filter(|l| l.starts_with("=== "))
    }

    fn tier_with(embed_ready: bool, state: capability::EmbedderState) -> capability::Tier {
        let mut caps = capability::Capabilities::all();
        caps.index_embed = embed_ready;
        capability::Tier::Server {
            url: "http://127.0.0.1:4655".to_string(),
            caps,
            auto_discovered: true,
            embedder_state: state,
            server_limits: None,
        }
    }

    #[test]
    fn detach_eligible_when_embedder_ready() {
        assert!(detach_embed_eligible(&tier_with(
            true,
            capability::EmbedderState::Ready
        )));
    }

    #[test]
    fn detach_eligible_when_embedder_still_loading() {
        assert!(detach_embed_eligible(&tier_with(
            false,
            capability::EmbedderState::Loading
        )));
    }

    #[test]
    fn detach_not_eligible_for_terminal_embedder_states() {
        for state in [
            capability::EmbedderState::Unavailable,
            capability::EmbedderState::Disabled,
            capability::EmbedderState::Unknown,
        ] {
            assert!(
                !detach_embed_eligible(&tier_with(false, state)),
                "state {state:?} is terminal; spawning a worker would wait forever"
            );
        }
    }

    #[test]
    fn detach_not_eligible_offline() {
        assert!(!detach_embed_eligible(&capability::Tier::Offline(
            capability::OfflineReason::NoLocalServer
        )));
    }

    #[test]
    fn handoff_confirm_timeout_is_2s() {
        assert_eq!(HANDOFF_CONFIRM_TIMEOUT.as_secs(), 2);
    }

    #[test]
    fn handoff_poll_interval_is_20ms() {
        assert_eq!(HANDOFF_POLL_INTERVAL.as_millis(), 20);
    }
}
