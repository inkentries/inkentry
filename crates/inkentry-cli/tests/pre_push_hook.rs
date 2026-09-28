// The ambient PATH deliberately omits the binary under test: the hook shim embeds an
// absolute path, so every test here also proves no PATH lookup is involved.

mod plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use assert_cmd::Command;
use std::path::{Path, PathBuf};
use std::process::Output;
use tempfile::TempDir;

fn inkentry_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_inkentry"))
}

// `git push` runs the hook, whose inkentry child loads config and a secret store from the
// environment, so all of it is pinned here rather than on the direct spawns. `HOME` alone
// does not pin the config dir: an ambient `INKENTRY_CONFIG_DIR` wins over it, and on
// Windows `dirs::home_dir()` reads no environment variable at all.
fn git_cmd(home: &Path, dir: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(dir)
        .env("HOME", home)
        .env("INKENTRY_CONFIG_DIR", home.join(".config").join("inkentry"))
        .env("INKENTRY_SECRET_STORE", "file")
        .env_remove("XDG_CONFIG_HOME")
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

#[cfg(unix)]
fn git_out_with_path(
    home: &Path,
    dir: &Path,
    path: impl AsRef<std::ffi::OsStr>,
    args: &[&str],
) -> Output {
    git_cmd(home, dir)
        .args(args)
        .env("PATH", path)
        .output()
        .expect("spawn git")
}

fn git_out(home: &Path, dir: &Path, args: &[&str]) -> Output {
    git_cmd(home, dir).args(args).output().expect("spawn git")
}

fn git(home: &Path, dir: &Path, args: &[&str]) -> Output {
    let out = git_out(home, dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

// Ignores exit status: a missing ref is a legitimate empty result.
fn git_stdout(home: &Path, dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&git_out(home, dir, args).stdout)
        .trim()
        .to_string()
}

fn bin(home: &Path, cwd: &Path) -> Command {
    let mut cmd = inkentry_bin_in(home);
    cmd.current_dir(cwd)
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL");
    cmd
}

// Repeats `inkentry_bin_in`'s isolation by hand because that always resolves the
// cargo-built binary; without the `INKENTRY_CONFIG_DIR` pin the runner's ambient value
// wins over `HOME`.
fn bin_at(exe: &Path, home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(exe);
    cmd.current_dir(cwd)
        .env("INKENTRY_SECRET_STORE", "file")
        .env("HOME", home)
        .env("INKENTRY_CONFIG_DIR", home.join(".config").join("inkentry"))
        .env_remove("XDG_CONFIG_HOME")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL");
    cmd
}

// Mirrors the command's env var so a rename there fails here instead of silently unguarding.
const NOTES_PUSH_SENTINEL: &str = "INKENTRY_NOTES_PUSH";

// The hook drops stdout, so the outcome is only reachable by running the command directly.
fn publish_notes_json(home: &Path, repo: &Path, remote: &str) -> serde_json::Value {
    let out = bin(home, repo)
        .args(["plumbing", "publish-notes", remote])
        .output()
        .expect("run publish-notes");
    assert!(
        out.status.success(),
        "publish-notes failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .expect("publish-notes emits one JSON object")
}

fn memory_add(home: &Path, repo: &Path, title: &str) {
    bin(home, repo)
        .args([
            "memory", "add", "--kind", "decision", "--title", title, "--body", "why",
        ])
        .assert()
        .success();
}

fn install_pre_push(home: &Path, repo: &Path) -> PathBuf {
    install_pre_push_from(&inkentry_exe(), home, repo)
}

fn install_pre_push_from(exe: &Path, home: &Path, repo: &Path) -> PathBuf {
    bin_at(exe, home, repo)
        .args(["hooks", "install", "--pre-push"])
        .assert()
        .success();
    hook_path(repo)
}

fn hook_path(repo: &Path) -> PathBuf {
    repo.join(".git").join("hooks").join("pre-push")
}

fn bare_origin(home: &Path, dir: &Path) {
    git(home, dir, &["init", "-q", "--bare", "-b", "main"]);
}

// Single quotes reach Git Bash with backslashes intact, so a Windows path must be forward-slashed.
fn sh_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn write_executable(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(path).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(path, p).unwrap();
    }
}

// Rejects per ref, so the branch push is untouched.
fn reject_notes_and_count(origin: &Path, counter: &Path) {
    write_executable(
        &origin.join("hooks").join("update"),
        &format!(
            "#!/bin/sh\ncase \"$1\" in refs/notes/*) echo try >> '{}' ; exit 1 ;; esac\nexit 0\n",
            sh_path(counter)
        ),
    );
}

fn line_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

fn teammate_publishes(home: &Path, origin: &Path, dir: &Path, title: &str) -> String {
    clone_dev(home, origin, dir);
    memory_add(home, dir, title);
    git(
        home,
        dir,
        &[
            "push",
            "-q",
            "origin",
            "refs/notes/inkentry:refs/notes/inkentry",
        ],
    );
    git_stdout(home, dir, &["rev-parse", "HEAD"])
}

fn commit(home: &Path, dir: &Path, name: &str) {
    std::fs::write(dir.join(format!("{name}.txt")), name).unwrap();
    git(home, dir, &["add", "."]);
    git(home, dir, &["commit", "-q", "-m", name]);
}

fn seed_origin(home: &Path, origin: &Path, dir: &Path) {
    git(home, dir, &["init", "-q", "-b", "main"]);
    git(home, dir, &["config", "user.email", "t@example.com"]);
    git(home, dir, &["config", "user.name", "Test"]);
    git(
        home,
        dir,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    commit(home, dir, "seed");
    git(home, dir, &["push", "-q", "-u", "origin", "main"]);
}

fn clone_dev(home: &Path, origin: &Path, dir: &Path) {
    git(
        home,
        dir.parent().unwrap(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            dir.to_str().unwrap(),
        ],
    );
    git(home, dir, &["config", "user.email", "t2@example.com"]);
    git(home, dir, &["config", "user.name", "Test2"]);
}

fn note_lines(home: &Path, dir: &Path, object: &str) -> Vec<String> {
    git_stdout(home, dir, &["notes", "--ref=inkentry", "show", object])
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

// Inserted ahead of the `exec` so a re-entry is counted even though the command's own
// sentinel would exit it early: this counts git invoking the hook, which `--no-verify`
// must prevent.
fn instrument_hook(hook_path: &Path, counter: &Path) {
    let body = std::fs::read_to_string(hook_path).unwrap();
    let (shebang, rest) = body.split_once('\n').expect("hook starts with a shebang");
    std::fs::write(
        hook_path,
        format!("{shebang}\necho fired >> '{}'\n{rest}", sh_path(counter)),
    )
    .unwrap();
}

fn fire_count(counter: &Path) -> usize {
    line_count(counter)
}

fn origin_and_dev(home: &Path, tmp: &Path) -> (PathBuf, PathBuf) {
    let origin = tmp.join("origin.git");
    let dev = tmp.join("dev");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&dev).unwrap();
    bare_origin(home, &origin);
    seed_origin(home, &origin, &dev);
    (origin, dev)
}

#[test]
fn hook_publishes_notes_and_fires_exactly_once() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    let hook = install_pre_push(home.path(), &dev);
    let counter = tmp.path().join("fires");
    instrument_hook(&hook, &counter);

    memory_add(home.path(), &dev, "recursion-guard-decision");
    let annotated = git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]);
    commit(home.path(), &dev, "second");
    git(home.path(), &dev, &["push", "-q", "origin", "main"]);

    assert_eq!(
        fire_count(&counter),
        1,
        "the hook must run exactly once per push; more means the notes push \
         re-entered it (the recursion the `--no-verify` guard prevents)"
    );

    // A guard that does nothing would also fire once, so check it published.
    assert!(
        !git_stdout(home.path(), &origin, &["rev-parse", "refs/notes/inkentry"]).is_empty(),
        "origin should carry refs/notes/inkentry after the push"
    );
    assert!(
        note_lines(home.path(), &origin, &annotated)
            .iter()
            .any(|l| l.contains("recursion-guard-decision")),
        "the pushed note should carry the recorded decision"
    );
}

#[test]
fn failed_notes_push_does_not_block_the_branch_push() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    let attempts = tmp.path().join("attempts");
    reject_notes_and_count(&origin, &attempts);

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "rejected-notes-decision");
    commit(home.path(), &dev, "payload");

    let out = git_out(home.path(), &dev, &["push", "origin", "main"]);
    assert!(
        out.status.success(),
        "the branch push must survive a rejected notes push, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]),
        git_stdout(home.path(), &origin, &["rev-parse", "refs/heads/main"]),
        "origin must have received the branch commit"
    );

    assert!(
        !git_out(home.path(), &origin, &["rev-parse", "refs/notes/inkentry"])
            .status
            .success(),
        "the rejected notes ref must not exist on origin"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("could not publish memory notes"),
        "the hook should warn on stderr, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A rejection is not a lost race: retrying it would park the push behind network timeouts.
    assert_eq!(
        line_count(&attempts),
        1,
        "a rejected notes push must be attempted exactly once, never retried"
    );
}

// The seeded config must be the ambient one, which is what the hook's child hits (it takes
// no `--config`); `git_cmd` pins `INKENTRY_CONFIG_DIR` at the seeded directory.
#[test]
fn an_unloadable_config_does_not_block_the_branch_push() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "broken-config-decision");
    let annotated = git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]);
    commit(home.path(), &dev, "payload");

    // Broken only now: `memory add` above needs a config that loads.
    let cfg_dir = home.path().join(".config").join("inkentry");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "not = valid toml [[[\n").unwrap();

    let out = git_out(home.path(), &dev, &["push", "origin", "main"]);
    assert!(
        out.status.success(),
        "the branch push must survive a config that will not load, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Without this the assert above passes even when the hook is never reached.
    assert_eq!(
        git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]),
        git_stdout(home.path(), &origin, &["rev-parse", "refs/heads/main"]),
        "origin must have received the branch commit"
    );

    assert!(
        note_lines(home.path(), &origin, &annotated)
            .iter()
            .any(|l| l.contains("broken-config-decision")),
        "the note must still publish despite the unloadable config"
    );

    assert!(
        String::from_utf8_lossy(&out.stderr).contains("config.toml"),
        "the hook should warn about the config on stderr, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// Same tolerance reached through `--config` rather than the ambient dir the hook's child uses.
#[test]
fn a_broken_config_is_tolerated_for_a_best_effort_publish() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (_origin, dev) = origin_and_dev(home.path(), tmp.path());

    let cfg = tmp.path().join("broken-config.toml");
    std::fs::write(&cfg, "not = valid toml [[[\n").unwrap();

    let out = bin(home.path(), &dev)
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "publish-notes", "--best-effort", "origin"])
        .output()
        .expect("run publish-notes");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a broken config must not fail a best-effort publish, got {:?}: {stderr}",
        out.status.code()
    );
    assert!(
        stderr.contains("config.toml"),
        "the config must be warned about rather than passing in silence, got: {stderr}"
    );

    let strict = bin(home.path(), &dev)
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "publish-notes", "origin"])
        .output()
        .expect("run publish-notes");
    assert!(
        !strict.status.success(),
        "a broken config must still fail a publish without --best-effort"
    );
}

#[test]
fn a_removed_binary_stops_the_push() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    // Install from a copy so the shim embeds a path we can remove.
    // The exact exit status is the shell's (126 bash, 127 dash); only non-zero makes git abort.
    let copy = tmp
        .path()
        .join(format!("inkentry-copy{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(inkentry_exe(), &copy).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    install_pre_push_from(&copy, home.path(), &dev);
    memory_add(home.path(), &dev, "removed-binary-decision");

    std::fs::remove_file(&copy).unwrap();

    commit(home.path(), &dev, "payload");
    let out = git_out(home.path(), &dev, &["push", "origin", "main"]);
    assert!(
        !out.status.success(),
        "a removed inkentry must stop the push rather than fail silently"
    );
    assert_ne!(
        git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]),
        git_stdout(home.path(), &origin, &["rev-parse", "refs/heads/main"]),
        "the push must not have proceeded"
    );
}

#[cfg(unix)]
#[test]
fn publishes_with_inkentry_absent_from_path() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "no-path-decision");
    let annotated = git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]);
    commit(home.path(), &dev, "no-path");

    // launchd-launched GUI clients lack `~/.local/bin`: a PATH holding only git.
    let bin_dir = tmp.path().join("git-only-bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let git_path = String::from_utf8(
        std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .expect("locate git")
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    std::os::unix::fs::symlink(&git_path, bin_dir.join("git")).unwrap();

    let out = git_out_with_path(
        home.path(),
        &dev,
        bin_dir.display().to_string(),
        &["push", "origin", "main"],
    );
    assert!(
        out.status.success(),
        "the push must succeed with inkentry off PATH: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        note_lines(home.path(), &origin, &annotated)
            .iter()
            .any(|l| l.contains("no-path-decision")),
        "publishing must not depend on a PATH lookup: a GUI client's PATH has no \
         ~/.local/bin, and those users must still publish"
    );
}

// The first fetch is served a stale view (before the teammate published), so the first push
// is genuinely non-fast-forward and the second fetch sees the teammate's notes.
#[cfg(unix)]
#[test]
fn a_lost_race_is_retried_and_converges_with_no_loss() {
    let home = TempDir::new().unwrap();
    let home2 = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());
    let stale = tmp.path().join("stale.git");
    let teammate = tmp.path().join("teammate");

    git(
        home.path(),
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            origin.to_str().unwrap(),
            stale.to_str().unwrap(),
        ],
    );

    let shared = teammate_publishes(home2.path(), &origin, &teammate, "teammate-raced-decision");
    assert_eq!(
        shared,
        git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]),
        "setup: both sides must annotate the same commit"
    );

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "dev-raced-decision");

    let stamp = tmp.path().join("served-stale");
    let calls = tmp.path().join("upload-pack-calls");
    let wrapper = tmp.path().join("uploadpack.sh");
    write_executable(
        &wrapper,
        &format!(
            "#!/bin/sh\n\
             echo call >> '{}'\n\
             if [ -f '{}' ]; then exec git upload-pack '{}'; fi\n\
             : > '{}'\n\
             exec git upload-pack '{}'\n",
            calls.display(),
            stamp.display(),
            origin.display(),
            stamp.display(),
            stale.display(),
        ),
    );
    git(
        home.path(),
        &dev,
        &[
            "config",
            "remote.origin.uploadpack",
            wrapper.to_str().unwrap(),
        ],
    );

    commit(home.path(), &dev, "raced");
    let out = git_out(home.path(), &dev, &["push", "origin", "main"]);
    assert!(
        out.status.success(),
        "the branch push must survive the race: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        line_count(&calls),
        2,
        "the command should fetch twice: the first push loses the race, the retry wins"
    );

    let published = note_lines(home.path(), &origin, &shared);
    assert!(
        published
            .iter()
            .any(|l| l.contains("teammate-raced-decision")),
        "the teammate's entry must survive our retry: {published:?}"
    );
    assert!(
        published.iter().any(|l| l.contains("dev-raced-decision")),
        "our entry must land on the retry: {published:?}"
    );
}

// A plain 2-dev divergence test cannot catch a force-push (the union merge carries both
// sides first); breaking the fetch is what makes the local ref genuinely diverge.
#[cfg(unix)]
#[test]
fn a_fetch_failure_must_not_destroy_a_teammates_notes() {
    let home = TempDir::new().unwrap();
    let home2 = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());
    let teammate = tmp.path().join("teammate");

    let shared = teammate_publishes(
        home2.path(),
        &origin,
        &teammate,
        "teammate-published-decision",
    );

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "dev-unmergeable-decision");

    // Break only the fetch: push rides receive-pack and is unaffected.
    git(
        home.path(),
        &dev,
        &[
            "config",
            "remote.origin.uploadpack",
            "/nonexistent/upload-pack",
        ],
    );

    commit(home.path(), &dev, "payload");
    let out = git_out(home.path(), &dev, &["push", "origin", "main"]);
    assert!(
        out.status.success(),
        "the branch push must survive a broken fetch: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]),
        git_stdout(home.path(), &origin, &["rev-parse", "refs/heads/main"]),
        "origin must have received the branch commit"
    );

    let published = note_lines(home.path(), &origin, &shared);
    assert!(
        published
            .iter()
            .any(|l| l.contains("teammate-published-decision")),
        "a fetch failure must never overwrite the teammate's published notes: {published:?}"
    );
}

// Proven by contention: with the lock held elsewhere the command blocks on the lock
// budget, where an unlocked merge would return immediately.
#[test]
fn the_publish_path_takes_the_notes_lock() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (_origin, dev) = origin_and_dev(home.path(), tmp.path());
    memory_add(home.path(), &dev, "locked-decision");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let held = rt
        .block_on(inkentry_core::storage::lock_notes(Some(&dev)))
        .expect("setup: the notes lock must be free to start with");

    let start = std::time::Instant::now();
    bin(home.path(), &dev)
        .args(["plumbing", "publish-notes", "origin"])
        .assert()
        .success();
    let contended = start.elapsed();
    drop(held);

    let start = std::time::Instant::now();
    bin(home.path(), &dev)
        .args(["plumbing", "publish-notes", "origin"])
        .assert()
        .success();
    let free = start.elapsed();

    assert!(
        contended >= std::time::Duration::from_secs(2),
        "publish must contend on the notes lock; it returned in {contended:?} with the \
         lock held, so its merge ran unlocked and a concurrent `memory add` could eat it \
         (uncontended run took {free:?})"
    );
}

// Diverged on purpose: converged, the push succeeds and every wrong answer still looks
// like success.
#[test]
fn a_publish_that_cannot_lock_skips_rather_than_misreporting_a_push() {
    let home1 = TempDir::new().unwrap();
    let home2 = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home1.path(), tmp.path());

    let dev2 = tmp.path().join("dev2");
    let shared = teammate_publishes(home2.path(), &origin, &dev2, "teammate-decision");
    memory_add(home1.path(), &dev, "our-decision");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let held = rt
        .block_on(inkentry_core::storage::lock_notes(Some(&dev)))
        .expect("setup: the notes lock must be free to start with");

    let out = bin(home1.path(), &dev)
        .args(["plumbing", "publish-notes", "origin"])
        .output()
        .expect("run publish-notes");
    drop(held);

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        out.status.success(),
        "a contended publish must not fail the caller, got {:?}: {stderr}",
        out.status.code()
    );

    let json: serde_json::Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .expect("publish-notes emits one JSON object");
    assert_eq!(
        json["published"], false,
        "a skipped merge must not be reported as published: {json}"
    );
    assert_eq!(
        json["skipped"], "lock_unavailable",
        "the skip must name its reason: {json}"
    );

    // The hook drops stdout, so stderr is the only channel that reaches a user.
    assert!(
        stderr.contains("lock"),
        "the skip must reach the user on stderr, got: {stderr}"
    );
    assert!(
        !stderr.contains("non-fast-forward"),
        "a skipped merge must not push: the rejection and its retry hint describe \
         a race that never happened, got: {stderr}"
    );

    let on_origin = note_lines(home2.path(), &origin, &shared);
    assert!(
        on_origin.iter().any(|l| l.contains("teammate-decision")),
        "a skipped publish must not disturb the teammate's entry: {on_origin:?}"
    );
    assert!(
        !on_origin.iter().any(|l| l.contains("our-decision")),
        "a skipped publish must not push: ours stays local until the next push, \
         got: {on_origin:?}"
    );
}

#[test]
fn the_notes_merge_strands_no_merge_worktree() {
    let home = TempDir::new().unwrap();
    let home2 = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());
    let teammate = tmp.path().join("teammate");

    let shared = teammate_publishes(home2.path(), &origin, &teammate, "their-decision");
    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "our-decision");

    commit(home.path(), &dev, "payload");
    git(home.path(), &dev, &["push", "-q", "origin", "main"]);

    assert!(
        !dev.join(".git").join("NOTES_MERGE_WORKTREE").exists(),
        "the merge must not strand a NOTES_MERGE_WORKTREE; the default `manual` \
         strategy does exactly that, which is why `-s cat_sort_uniq` is explicit"
    );
    assert!(
        !dev.join(".git").join("NOTES_MERGE_REF").exists(),
        "a stuck partial merge must not be left behind"
    );

    let merged = note_lines(home.path(), &dev, &shared);
    assert!(
        merged.iter().any(|l| l.contains("their-decision"))
            && merged.iter().any(|l| l.contains("our-decision")),
        "the union must resolve the conflict, keeping both sides: {merged:?}"
    );
}

#[test]
fn two_dev_divergence_converges_with_no_loss() {
    let home1 = TempDir::new().unwrap();
    let home2 = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev1) = origin_and_dev(home1.path(), tmp.path());
    let dev2 = tmp.path().join("dev2");
    clone_dev(home2.path(), &origin, &dev2);

    let shared = git_stdout(home1.path(), &dev1, &["rev-parse", "HEAD"]);
    assert_eq!(
        shared,
        git_stdout(home2.path(), &dev2, &["rev-parse", "HEAD"])
    );

    install_pre_push(home1.path(), &dev1);
    install_pre_push(home2.path(), &dev2);
    memory_add(home1.path(), &dev1, "dev1-only-decision");
    memory_add(home2.path(), &dev2, "dev2-only-decision");

    git(home1.path(), &dev1, &["checkout", "-q", "-b", "feature-1"]);
    commit(home1.path(), &dev1, "one");
    git(home1.path(), &dev1, &["push", "-q", "origin", "feature-1"]);

    git(home2.path(), &dev2, &["checkout", "-q", "-b", "feature-2"]);
    commit(home2.path(), &dev2, "two");
    git(home2.path(), &dev2, &["push", "-q", "origin", "feature-2"]);

    let merged = note_lines(home2.path(), &dev2, &shared);
    assert!(
        merged.iter().any(|l| l.contains("dev1-only-decision")),
        "dev2 must have merged dev1's entry rather than replacing it: {merged:?}"
    );
    assert!(
        merged.iter().any(|l| l.contains("dev2-only-decision")),
        "dev2 must still have its own entry: {merged:?}"
    );

    git(
        home1.path(),
        &dev1,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/notes/inkentry:refs/notes/origin/inkentry",
        ],
    );
    git(
        home1.path(),
        &dev1,
        &[
            "notes",
            "--ref=inkentry",
            "merge",
            "-s",
            "cat_sort_uniq",
            "refs/notes/origin/inkentry",
        ],
    );
    let round_tripped = note_lines(home1.path(), &dev1, &shared);
    assert!(
        round_tripped
            .iter()
            .any(|l| l.contains("dev2-only-decision")),
        "dev1 must receive dev2's entry: {round_tripped:?}"
    );
    assert!(
        round_tripped
            .iter()
            .any(|l| l.contains("dev1-only-decision")),
        "dev1 must keep its own entry: {round_tripped:?}"
    );
}

// git newline-terminating note bodies is what keeps `cat_sort_uniq` from welding records; it
// is owned by git, so pin it. A substring assertion cannot: a welded line has both titles.
#[test]
fn the_union_welds_no_records_together() {
    let home = TempDir::new().unwrap();
    let home2 = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());
    let teammate = tmp.path().join("teammate");

    let shared = teammate_publishes(home2.path(), &origin, &teammate, "their-decision");
    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "our-decision");

    commit(home.path(), &dev, "payload");
    git(home.path(), &dev, &["push", "-q", "origin", "main"]);

    let merged = note_lines(home.path(), &dev, &shared);
    assert!(
        merged.len() >= 2,
        "the union must keep each record on its own line: {merged:?}"
    );
    for line in &merged {
        if let Err(e) = serde_json::from_str::<serde_json::Value>(line) {
            panic!(
                "every merged line must parse as one whole record, but {line:?} did not ({e}); \
                 git no longer newline-terminates note bodies, so the union welds records"
            );
        }
    }
}

#[test]
fn repeated_syncs_are_idempotent() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "idempotent-decision");
    let annotated = git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]);

    for i in 0..3 {
        commit(home.path(), &dev, &format!("push-{i}"));
        let out = git_out(home.path(), &dev, &["push", "origin", "main"]);
        assert!(
            out.status.success(),
            "push {i} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let lines = note_lines(home.path(), &dev, &annotated);
    let hits = lines
        .iter()
        .filter(|l| l.contains("idempotent-decision"))
        .count();
    assert_eq!(
        hits, 1,
        "repeated syncs must not duplicate an entry: {lines:?}"
    );
    assert_eq!(
        lines,
        note_lines(home.path(), &origin, &annotated),
        "local and origin notes must have converged"
    );
}

#[test]
fn skips_gracefully_with_no_local_notes_ref() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    install_pre_push(home.path(), &dev);
    assert!(
        !git_out(
            home.path(),
            &dev,
            &["rev-parse", "--verify", "refs/notes/inkentry"]
        )
        .status
        .success(),
        "setup: no notes recorded yet"
    );

    commit(home.path(), &dev, "no-notes");
    let out = git_out(home.path(), &dev, &["push", "origin", "main"]);
    assert!(
        out.status.success(),
        "push must succeed with no notes to publish: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !git_out(home.path(), &origin, &["rev-parse", "refs/notes/inkentry"])
            .status
            .success(),
        "an empty notes ref must not be invented on origin"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("inkentry:"),
        "a no-op must be silent, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// A surviving push cannot stand in for the skip: a URL pushes just as well, so dropping the
// guard also leaves the push green. Both halves of the skip are asserted instead.
#[test]
fn skips_gracefully_when_pushing_without_a_named_remote() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "by-url-decision");
    commit(home.path(), &dev, "by-url");

    let out = git_out(
        home.path(),
        &dev,
        &["push", origin.to_str().unwrap(), "main"],
    );
    assert!(
        out.status.success(),
        "a push by URL must still succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !git_out(home.path(), &origin, &["rev-parse", "refs/notes/inkentry"])
            .status
            .success(),
        "a push by URL must publish nothing: the flow skips rather than resolving \
         the URL itself"
    );

    assert_eq!(
        publish_notes_json(home.path(), &dev, origin.to_str().unwrap())["skipped"],
        "no_such_remote",
        "a URL must skip as an unresolvable remote"
    );
}

// `--no-verify` is the guard that holds; this pins the sentinel backstop for a client that
// runs the hook regardless. Driven at the command layer because nothing reaches it
// through a real `git push`.
#[test]
fn a_re_entered_publish_stops_at_the_sentinel() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    memory_add(home.path(), &dev, "sentinel-decision");

    let out = bin(home.path(), &dev)
        .args(["plumbing", "publish-notes", "origin"])
        .env(NOTES_PUSH_SENTINEL, "1")
        .output()
        .expect("run publish-notes");
    assert!(
        out.status.success(),
        "a re-entered publish must exit 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let reported: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
            .expect("publish-notes emits one JSON object");
    assert_eq!(
        reported["skipped"], "recursion",
        "a re-entered publish must report the recursion skip: {reported}"
    );
    assert!(
        !git_out(home.path(), &origin, &["rev-parse", "refs/notes/inkentry"])
            .status
            .success(),
        "a re-entered publish must push nothing"
    );
}

#[test]
fn publishes_to_the_remote_being_pushed_to() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let upstream = tmp.path().join("upstream.git");
    let dev = tmp.path().join("dev");
    std::fs::create_dir_all(&upstream).unwrap();
    std::fs::create_dir_all(&dev).unwrap();
    bare_origin(home.path(), &upstream);

    git(home.path(), &dev, &["init", "-q", "-b", "main"]);
    git(
        home.path(),
        &dev,
        &["config", "user.email", "t@example.com"],
    );
    git(home.path(), &dev, &["config", "user.name", "Test"]);
    git(
        home.path(),
        &dev,
        &["remote", "add", "upstream", upstream.to_str().unwrap()],
    );
    commit(home.path(), &dev, "seed");
    git(home.path(), &dev, &["push", "-q", "-u", "upstream", "main"]);
    assert!(
        !git_out(home.path(), &dev, &["remote", "get-url", "origin"])
            .status
            .success(),
        "setup: this repo must have no origin remote"
    );

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "upstream-decision");
    let annotated = git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]);
    commit(home.path(), &dev, "payload");
    let out = git_out(home.path(), &dev, &["push", "upstream", "main"]);
    assert!(
        out.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        note_lines(home.path(), &upstream, &annotated)
            .iter()
            .any(|l| l.contains("upstream-decision")),
        "the notes must reach the remote being pushed to, not a hardcoded 'origin'"
    );
}

#[test]
fn install_bails_on_a_foreign_pre_push_hook() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (_origin, dev) = origin_and_dev(home.path(), tmp.path());

    let hook = hook_path(&dev);
    let foreign = "#!/bin/sh\necho someone else's hook\n";
    write_executable(&hook, foreign);

    bin(home.path(), &dev)
        .args(["hooks", "install", "--pre-push"])
        .assert()
        .failure();

    assert_eq!(
        std::fs::read_to_string(&hook).unwrap(),
        foreign,
        "a foreign hook must be left byte-for-byte alone"
    );
}

#[cfg(unix)]
#[test]
fn installed_hook_is_executable() {
    use std::os::unix::fs::PermissionsExt;
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (_origin, dev) = origin_and_dev(home.path(), tmp.path());

    let hook = install_pre_push(home.path(), &dev);
    let mode = std::fs::metadata(&hook).unwrap().permissions().mode();
    assert_eq!(
        mode & 0o111,
        0o111,
        "git will not run a non-executable hook"
    );
}

#[test]
fn install_is_idempotent() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (_origin, dev) = origin_and_dev(home.path(), tmp.path());

    let hook = install_pre_push(home.path(), &dev);
    let first = std::fs::read_to_string(&hook).unwrap();

    bin(home.path(), &dev)
        .args(["hooks", "install", "--pre-push"])
        .assert()
        .success()
        .stdout(predicates::str::contains("already installed"));

    assert_eq!(
        std::fs::read_to_string(&hook).unwrap(),
        first,
        "a re-install must not churn the hook"
    );
}

#[test]
fn install_re_resolves_a_moved_binary() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (_origin, dev) = origin_and_dev(home.path(), tmp.path());

    let old = tmp
        .path()
        .join(format!("old-inkentry{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(inkentry_exe(), &old).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let hook = install_pre_push_from(&old, home.path(), &dev);
    assert!(
        std::fs::read_to_string(&hook)
            .unwrap()
            .contains(&sh_path(&old)),
        "setup: the shim must embed the path it was installed from"
    );

    // The marker still matches, so a marker-only idempotence check would skip the rewrite.
    install_pre_push(home.path(), &dev);
    let body = std::fs::read_to_string(&hook).unwrap();
    assert!(
        body.contains(&sh_path(&inkentry_exe())),
        "re-installing must re-resolve the binary path: {body}"
    );
    assert!(
        !body.contains(&sh_path(&old)),
        "the stale path must be gone: {body}"
    );
}

#[test]
fn uninstall_removes_the_pre_push_hook() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (_origin, dev) = origin_and_dev(home.path(), tmp.path());

    let hook = install_pre_push(home.path(), &dev);
    assert!(hook.exists());

    bin(home.path(), &dev)
        .args(["hooks", "uninstall"])
        .assert()
        .success();

    assert!(!hook.exists(), "uninstall must remove the pre-push hook");
}

#[test]
fn uninstall_leaves_a_foreign_hook_alone() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (_origin, dev) = origin_and_dev(home.path(), tmp.path());

    let pre_push = install_pre_push(home.path(), &dev);
    let post_commit = dev.join(".git").join("hooks").join("post-commit");
    let foreign = "#!/bin/sh\necho someone else's hook\n";
    write_executable(&post_commit, foreign);

    bin(home.path(), &dev)
        .args(["hooks", "uninstall"])
        .assert()
        .success();

    assert!(!pre_push.exists(), "our hook must go");
    assert_eq!(
        std::fs::read_to_string(&post_commit).unwrap(),
        foreign,
        "a foreign hook must survive uninstall untouched"
    );
}
