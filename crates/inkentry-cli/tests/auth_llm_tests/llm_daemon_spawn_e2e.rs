// Runs `inkentry server start` against a recording stand-in for the daemon and
// asserts on the argv and environment it received. Unit tests on either side
// cannot see a call site that stops resolving, or a variable the child inherits
// behind the CLI's back.

#![cfg(unix)]

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tempfile::TempDir;

struct Spawned {
    argv: String,
    env: Vec<String>,
}

impl Spawned {
    fn env_value(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .find_map(|l| l.strip_prefix(&format!("{name}=")))
    }
}

// Never binds the port, so the CLI's start path sees the process end without
// serving.
fn recording_server(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let record = dir.join("record.txt");
    let bin = dir.join("recording-inkentry-server");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\n{{ echo \"ARGV $*\"; env; }} > '{}'\n",
            record.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, record)
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

fn start_daemon(config_toml: &str, env: &[(&str, &str)], extra_args: &[&str]) -> Spawned {
    let home = TempDir::new().unwrap().keep();
    let (bin, record) = recording_server(&home);
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, config_toml).unwrap();

    let mut cmd = inkentry_bin_in(&home);
    cmd.env("INKENTRY_STATE_DIR", home.join("state"))
        .env_remove("INKENTRY_LLM_URL")
        .env_remove("INKENTRY_LLM_MODEL")
        .env_remove("INKENTRY_LLM_KEY");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd
        .arg("--config")
        .arg(&config_path)
        .args(["server", "start", "--port"])
        .arg(free_port().to_string())
        .arg("--bin")
        .arg(&bin)
        .arg("--db")
        .arg(home.join("server.db"))
        .args(extra_args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let recorded = std::fs::read_to_string(&record)
        .unwrap_or_else(|e| panic!("the daemon stand-in recorded nothing ({e})"));
    let mut lines = recorded.lines().map(str::to_string);
    let argv = lines.next().unwrap_or_default();
    Spawned {
        argv,
        env: lines.collect(),
    }
}

#[test]
fn a_configured_endpoint_reaches_the_spawned_daemon() {
    let spawned = start_daemon(
        "llm_url = \"http://endpoint.invalid:1234\"\nllm_model = \"from-config\"\n",
        &[],
        &[],
    );

    assert!(
        spawned
            .argv
            .contains("--llm-url http://endpoint.invalid:1234"),
        "the configured endpoint never reached the daemon: {}",
        spawned.argv
    );
    assert!(
        spawned.argv.contains("--llm-model from-config"),
        "the configured model never reached the daemon: {}",
        spawned.argv
    );
}

#[test]
fn an_environment_endpoint_reaches_the_spawned_daemon() {
    let spawned = start_daemon(
        "",
        &[
            ("INKENTRY_LLM_URL", "http://from-env.invalid:1234"),
            ("INKENTRY_LLM_MODEL", "from-env"),
        ],
        &[],
    );

    assert!(
        spawned
            .argv
            .contains("--llm-url http://from-env.invalid:1234"),
        "got {}",
        spawned.argv
    );
    assert!(
        spawned.argv.contains("--llm-model from-env"),
        "got {}",
        spawned.argv
    );
}

#[test]
fn the_start_flags_outrank_both_the_environment_and_the_config() {
    let spawned = start_daemon(
        "llm_url = \"http://from-config.invalid:1234\"\nllm_model = \"from-config\"\n",
        &[
            ("INKENTRY_LLM_URL", "http://from-env.invalid:1234"),
            ("INKENTRY_LLM_MODEL", "from-env"),
        ],
        &[
            "--llm-url",
            "http://from-flag.invalid:1234",
            "--llm-model",
            "from-flag",
        ],
    );

    assert!(
        spawned
            .argv
            .contains("--llm-url http://from-flag.invalid:1234"),
        "the flag must win: {}",
        spawned.argv
    );
    assert!(
        spawned.argv.contains("--llm-model from-flag"),
        "the flag must win: {}",
        spawned.argv
    );
    assert!(
        !spawned.argv.contains("from-env") && !spawned.argv.contains("from-config"),
        "an outranked value must not reach the daemon at all: {}",
        spawned.argv
    );
    assert_eq!(
        spawned.env_value("INKENTRY_LLM_URL"),
        Some("http://from-flag.invalid:1234"),
        "the child's inherited variable must be replaced, not left as the parent's"
    );
    assert_eq!(spawned.env_value("INKENTRY_LLM_MODEL"), Some("from-flag"));
}

// The CLI omitting the argument is not enough: `inkentry-server` reads
// `INKENTRY_LLM_URL` through clap `env`, so an inherited empty value arrives as
// a present-but-empty endpoint.
#[test]
fn an_exported_empty_endpoint_leaves_the_daemon_with_no_llm_at_all() {
    let spawned = start_daemon(
        "llm_url = \"http://from-config.invalid:1234\"\nllm_model = \"from-config\"\n",
        &[("INKENTRY_LLM_URL", ""), ("INKENTRY_LLM_MODEL", "")],
        &[],
    );

    assert!(
        !spawned.argv.contains("--llm-url"),
        "a blanked endpoint must emit no argument: {}",
        spawned.argv
    );
    assert_eq!(
        spawned.env_value("INKENTRY_LLM_URL"),
        None,
        "the child inherited the blank endpoint, which its own clap env binding \
         then reads as a configured one"
    );
    assert_eq!(spawned.env_value("INKENTRY_LLM_MODEL"), None);
}

#[test]
fn a_model_without_an_endpoint_reaches_the_daemon_on_neither_channel() {
    let spawned = start_daemon("llm_model = \"orphan\"\n", &[], &[]);

    assert!(
        !spawned.argv.contains("--llm-model"),
        "got {}",
        spawned.argv
    );
    assert_eq!(spawned.env_value("INKENTRY_LLM_MODEL"), None);
    assert_eq!(spawned.env_value("INKENTRY_LLM_URL"), None);
}

// Asserted on the whole argv, not on a flag name.
#[test]
fn the_credential_reaches_the_child_environment_and_never_its_argv() {
    let spawned = start_daemon(
        "llm_url = \"http://endpoint.invalid:1234\"\n",
        &[("INKENTRY_LLM_KEY", "sk-endpoint-credential")],
        &[],
    );

    assert_eq!(
        spawned.env_value("INKENTRY_LLM_KEY"),
        Some("sk-endpoint-credential")
    );
    assert!(
        !spawned.argv.contains("sk-endpoint-credential"),
        "the credential must never reach the process table: {}",
        spawned.argv
    );
}

// A daemon that exits immediately rejected its own configuration; blaming a
// firewall, or waiting out the liveness timeout first, misleads the user.
#[test]
fn a_daemon_that_exits_immediately_is_not_reported_as_a_firewall_problem() {
    let home = TempDir::new().unwrap().keep();
    let (bin, _record) = recording_server(&home);
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, "").unwrap();

    let started = std::time::Instant::now();
    let out = inkentry_bin_in(&home)
        .env("INKENTRY_STATE_DIR", home.join("state"))
        .arg("--config")
        .arg(&config_path)
        .args(["server", "start", "--port"])
        .arg(free_port().to_string())
        .arg("--bin")
        .arg(&bin)
        .arg("--db")
        .arg(home.join("server.db"))
        .output()
        .unwrap();
    let elapsed = started.elapsed();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("firewall"),
        "the process is gone, so the network is not the diagnosis: {stderr}"
    );
    assert!(
        stderr.contains("exited immediately"),
        "the user needs to be sent to the log, not to their firewall settings: {stderr}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "waiting out the full liveness timeout for a process already gone: {elapsed:?}"
    );
}
