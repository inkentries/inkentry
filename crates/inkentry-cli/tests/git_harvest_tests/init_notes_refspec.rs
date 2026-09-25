use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin;

use predicates::prelude::*;
use std::path::Path;
use std::process::Output;
use tempfile::tempdir;

const NOTES_REFSPEC: &str = "+refs/notes/inkentry*:refs/notes/origin/inkentry*";

// A fetch lands on a tracking ref, never the working ref: fetching straight onto `refs/notes/inkentry`
// would force-update it and destroy local unpushed notes.
const TRACKING_REF: &str = "refs/notes/origin/inkentry";

fn git(dir: &Path, args: &[&str]) {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> Output {
    std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git")
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&git_out(dir, args).stdout)
        .trim()
        .to_string()
}

// Local identity so spawned `git` (and inkentry's inner git) can commit without the runner's global config.
fn init_repo_with_commit(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "# test\n").unwrap();
    git(dir, &["add", "README.md"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn empty_config(dir: &Path) -> std::path::PathBuf {
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, "").unwrap();
    cfg
}

fn run_init(dir: &Path) -> String {
    let cfg = empty_config(dir);
    let out = inkentry_bin()
        .current_dir(dir)
        .env("HOME", dir)
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&cfg)
        .args(["init", "--no-index"])
        .output()
        .expect("spawn inkentry init");
    assert!(
        out.status.success(),
        "inkentry init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn init_configures_notes_refspec_when_origin_present() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let origin = tmp.path().join("origin.git");
    std::fs::create_dir_all(&repo).unwrap();

    git(
        tmp.path(),
        &[
            "init",
            "--bare",
            "-q",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    init_repo_with_commit(&repo);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    let stdout = run_init(&repo);

    let fetch = git_stdout(&repo, &["config", "--get-all", "remote.origin.fetch"]);
    assert!(
        fetch.lines().any(|l| l.trim() == NOTES_REFSPEC),
        "remote.origin.fetch should contain the notes refspec, got:\n{fetch}"
    );
    assert!(
        stdout.contains("Memory:") && stdout.contains("configured notes fetch refspec on 'origin'"),
        "init stdout should announce the configured refspec, got:\n{stdout}"
    );
}

#[test]
fn init_no_origin_prints_hint_and_succeeds() {
    let tmp = tempdir().unwrap();
    init_repo_with_commit(tmp.path());

    let stdout = run_init(tmp.path());

    assert!(
        stdout.contains(&format!(
            "git config --add remote.origin.fetch '{NOTES_REFSPEC}'"
        )),
        "no-origin init should print the exact refspec hint, got:\n{stdout}"
    );
    // Publishing is opt-in, so init must name the step unprompted.
    assert!(
        stdout.contains("your memory stays local until you install the pre-push hook")
            && stdout.contains("inkentry hooks install --pre-push"),
        "no-origin init should name the pre-push hook as the publishing step, got:\n{stdout}"
    );
    // Pushing notes after each change orphans entries recorded before their commit is pushed.
    assert!(
        !stdout.contains("push notes after each memory change"),
        "init must not advertise the orphan-prone per-change notes push, got:\n{stdout}"
    );
    assert!(
        !git_out(tmp.path(), &["remote", "get-url", "origin"])
            .status
            .success(),
        "init must not create an origin remote when none exists"
    );
}

#[test]
fn init_announce_reflects_the_installed_pre_push_hook() {
    let tmp = tempdir().unwrap();
    init_repo_with_commit(tmp.path());

    let before = run_init(tmp.path());
    assert!(
        before.contains("your memory stays local until you install the pre-push hook"),
        "with no hook installed, init must say memory stays local, got:\n{before}"
    );

    let cfg = empty_config(tmp.path());
    inkentry_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["hooks", "install", "--pre-push"])
        .assert()
        .success();

    let after = run_init(tmp.path());
    assert!(
        after.contains("pre-push hook installed: your memory publishes on `git push`"),
        "with the hook installed, init must report that, got:\n{after}"
    );
    assert!(
        !after.contains("your memory stays local"),
        "init must not still ask for an install it already has, got:\n{after}"
    );
}

#[test]
fn init_configures_notes_rewrite_ref_without_an_origin_remote() {
    let tmp = tempdir().unwrap();
    init_repo_with_commit(tmp.path());
    assert!(
        !git_out(tmp.path(), &["remote", "get-url", "origin"])
            .status
            .success(),
        "setup: this repo must have no origin remote"
    );

    let stdout = run_init(tmp.path());

    // Read with `--local` so the assertion covers what init wrote, not an ambient global value.
    assert_eq!(
        git_stdout(
            tmp.path(),
            &["config", "--local", "--get-all", "notes.rewriteRef"]
        )
        .trim(),
        "refs/notes/inkentry",
        "init must configure the notes carry ref even with no origin, got:\n{stdout}"
    );
    assert!(
        stdout.contains("configured notes.rewriteRef"),
        "init should announce the carry config, got:\n{stdout}"
    );
}

#[test]
fn init_notes_refspec_is_idempotent() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let origin = tmp.path().join("origin.git");
    std::fs::create_dir_all(&repo).unwrap();

    git(
        tmp.path(),
        &[
            "init",
            "--bare",
            "-q",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    init_repo_with_commit(&repo);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    run_init(&repo);
    let second = run_init(&repo);

    let count = git_stdout(&repo, &["config", "--get-all", "remote.origin.fetch"])
        .lines()
        .filter(|l| l.trim() == NOTES_REFSPEC)
        .count();
    assert_eq!(
        count, 1,
        "notes refspec must appear exactly once after two inits"
    );
    assert!(
        second.contains("already configured"),
        "second init should report the refspec is already configured, got:\n{second}"
    );
}

#[test]
fn init_does_not_set_origin_push_refspec() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let origin = tmp.path().join("origin.git");
    std::fs::create_dir_all(&repo).unwrap();

    git(
        tmp.path(),
        &[
            "init",
            "--bare",
            "-q",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    init_repo_with_commit(&repo);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    run_init(&repo);

    let push = git_out(&repo, &["config", "--get", "remote.origin.push"]);
    assert!(
        !push.status.success() && String::from_utf8_lossy(&push.stdout).trim().is_empty(),
        "remote.origin.push must remain unset, got: {:?}",
        String::from_utf8_lossy(&push.stdout)
    );
}

#[test]
fn notes_round_trip_through_bare_origin() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let origin = tmp.path().join("origin.git");
    let clone = tmp.path().join("clone");
    std::fs::create_dir_all(&repo).unwrap();

    git(
        tmp.path(),
        &[
            "init",
            "--bare",
            "-q",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    init_repo_with_commit(&repo);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    run_init(&repo);

    let mem_db = repo.join(".inkentry").join("memory.db");
    let cfg = repo.join("mem-config.toml");
    std::fs::write(
        &cfg,
        format!(
            "db_path = {:?}\nllm_model = \"x\"\nstore_in_git_notes = true\n",
            mem_db
        ),
    )
    .unwrap();

    let unique = "notes travel via the origin refspec";
    inkentry_bin()
        .current_dir(&repo)
        .env("HOME", &repo)
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL")
        .arg("--config")
        .arg(&cfg)
        .arg("memory")
        .arg("--db")
        .arg(&mem_db)
        .arg("add")
        .arg("--kind")
        .arg("decision")
        .arg("--title")
        .arg(unique)
        .arg("--body")
        .arg("Chosen so refs/notes/inkentry clone/fetch behaviour is observable.")
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [decision]"));

    assert!(
        !git_stdout(&repo, &["notes", "--ref=inkentry", "list"]).is_empty(),
        "expected a local inkentry note after memory add"
    );

    git(&repo, &["push", "-q", "origin", "main"]);
    git(&repo, &["push", "-q", "origin", "refs/notes/inkentry"]);

    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    // Identity for the inner git that inkentry runs in the clone.
    git(&clone, &["config", "user.email", "clone@example.com"]);
    git(&clone, &["config", "user.name", "Clone"]);
    assert!(
        git_stdout(&clone, &["notes", "--ref=inkentry", "list"]).is_empty(),
        "a fresh clone should not have inkentry notes before fetch"
    );

    // `run_init` also performs the first read-path merge, so assert on the tracking ref directly to
    // observe the fetch in isolation.
    run_init(&clone);
    git(&clone, &["fetch", "-q", "origin"]);

    assert!(
        git_out(&clone, &["rev-parse", "--verify", TRACKING_REF])
            .status
            .success(),
        "a plain fetch must populate {TRACKING_REF} via the init-configured refspec"
    );

    let tracking_notes = git_stdout(&clone, &["notes", &format!("--ref={TRACKING_REF}"), "list"]);
    let annotated = tracking_notes
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .expect("note list line has an annotated object")
        .to_string();
    let shown = git_stdout(
        &clone,
        &[
            "notes",
            &format!("--ref={TRACKING_REF}"),
            "show",
            &annotated,
        ],
    );
    assert!(
        shown.contains(unique),
        "fetched note should contain the decision title, got:\n{shown}"
    );

    let listed = inkentry_bin()
        .current_dir(&clone)
        .env("HOME", &clone)
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL")
        .args(["memory", "--backend", "git-notes", "list"])
        .output()
        .expect("spawn inkentry memory list");
    assert!(
        listed.status.success(),
        "memory list should succeed in the clone: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains(unique),
        "the read-path merge should surface the fetched decision, got:\n{}",
        String::from_utf8_lossy(&listed.stdout)
    );
    assert!(
        git_stdout(&clone, &["notes", "--ref=inkentry", "show", &annotated]).contains(unique),
        "the read-path merge should have folded the tracking ref into refs/notes/inkentry"
    );
}

// Stands in for a `git fetch` that landed a teammate's note on the tracking ref, without network access.
fn add_note_on_ref(dir: &Path, git_ref: &str, body: &str) {
    git(
        dir,
        &[
            "notes",
            &format!("--ref={git_ref}"),
            "add",
            "-f",
            "-m",
            body,
            "HEAD",
        ],
    );
}

fn working_note(dir: &Path) -> String {
    let out = git_out(dir, &["notes", "--ref=inkentry", "show", "HEAD"]);
    if out.status.success() {
        String::from_utf8_lossy(&out.stdout).into_owned()
    } else {
        String::new()
    }
}

#[test]
fn context_merges_the_tracking_ref_and_surfaces_a_fetched_entry() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo_with_commit(&repo);

    run_init(&repo);

    // A teammate's entry, as a fetch would have left it; plus one of my own, so
    // the refs genuinely diverge.
    const THEIRS: &str = r#"{"schema_version":1,"id":1,"kind":"decision","title":"their fetched decision","body":"b","tags":[],"linked_files":[],"created_at":100,"status":"active"}"#;
    const MINE: &str = r#"{"schema_version":1,"id":2,"kind":"decision","title":"my local decision","body":"b","tags":[],"linked_files":[],"created_at":200,"status":"active"}"#;
    add_note_on_ref(&repo, TRACKING_REF, THEIRS);
    add_note_on_ref(&repo, "refs/notes/inkentry", MINE);

    assert!(
        !working_note(&repo).contains("their fetched decision"),
        "setup: the fetched entry must start out on the tracking ref only"
    );

    let cfg = empty_config(&repo);
    let out = inkentry_bin()
        .current_dir(&repo)
        .env("HOME", &repo)
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL")
        .arg("--config")
        .arg(&cfg)
        .args(["context", "--backend", "git-notes"])
        .output()
        .expect("spawn inkentry context");
    assert!(
        out.status.success(),
        "inkentry context should succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("their fetched decision"),
        "context must merge the tracking ref and surface the fetched entry, got:\n{stdout}"
    );
    assert!(
        stdout.contains("my local decision"),
        "the union must not drop my local entry, got:\n{stdout}"
    );
    assert!(
        working_note(&repo).contains("their fetched decision"),
        "context's merge should have folded the tracking ref into refs/notes/inkentry"
    );
}

#[test]
fn init_merges_the_tracking_ref_before_importing_git_notes() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo_with_commit(&repo);

    // Arrives before this repo is ever init'd: the fresh-clone case.
    const THEIRS: &str = r#"{"schema_version":1,"id":1,"kind":"decision","title":"their fetched decision","body":"b","tags":[],"linked_files":[],"created_at":100,"status":"active"}"#;
    add_note_on_ref(&repo, TRACKING_REF, THEIRS);
    assert!(
        working_note(&repo).is_empty(),
        "setup: nothing may be on the working ref yet"
    );

    let stdout = run_init(&repo);

    assert!(
        working_note(&repo).contains("their fetched decision"),
        "init must merge the tracking ref onto refs/notes/inkentry"
    );
    // The merge fed the import; without it the import sees an empty working ref.
    assert!(
        stdout.contains("imported 1 entries from git notes"),
        "init must import the fetched entry it merged, got:\n{stdout}"
    );
}

#[test]
fn init_non_tty_does_not_prompt_or_hang() {
    let tmp = tempdir().unwrap();
    init_repo_with_commit(tmp.path());

    let stdout = run_init(tmp.path());
    assert!(
        stdout.contains("inkentry initialised for"),
        "init should print its success summary in non-TTY mode, got:\n{stdout}"
    );
}

// The non-glob refspec makes `git fetch` exit 128 when the remote has no notes ref; the glob tolerates it.
#[test]
fn init_leaves_plain_fetch_and_pull_working_with_no_notes_on_the_remote() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let origin = tmp.path().join("origin.git");
    std::fs::create_dir_all(&repo).unwrap();

    git(
        tmp.path(),
        &[
            "init",
            "--bare",
            "-q",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    init_repo_with_commit(&repo);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    // `-u` sets the upstream `git pull` needs; without it pull exits 1 for an unrelated reason.
    git(&repo, &["push", "-q", "-u", "origin", "main"]);

    run_init(&repo);

    let fetch = git_out(&repo, &["fetch", "origin"]);
    assert!(
        fetch.status.success(),
        "git fetch must still exit 0 after init when the remote has no notes, got {:?}: {}",
        fetch.status.code(),
        String::from_utf8_lossy(&fetch.stderr)
    );

    let pull = git_out(&repo, &["pull"]);
    assert!(
        pull.status.success(),
        "git pull must still exit 0 after init when the remote has no notes, got {:?}: {}",
        pull.status.code(),
        String::from_utf8_lossy(&pull.stderr)
    );
}

// A leading `+` refspec straight onto the working ref force-updates it and silently replaces a local
// unpushed note; a glob alone does not fix that, only the tracking destination does.
#[test]
fn local_unpushed_note_survives_a_fetch_when_the_remote_has_notes() {
    let tmp = tempdir().unwrap();
    let teammate = tmp.path().join("teammate");
    let origin = tmp.path().join("origin.git");
    let mine = tmp.path().join("mine");
    std::fs::create_dir_all(&teammate).unwrap();

    git(
        tmp.path(),
        &[
            "init",
            "--bare",
            "-q",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    init_repo_with_commit(&teammate);
    git(
        &teammate,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&teammate, &["push", "-q", "origin", "main"]);

    const THEIRS: &str = r#"{"schema_version":1,"id":1,"kind":"decision","title":"theirs"}"#;
    git(
        &teammate,
        &["notes", "--ref=inkentry", "add", "-f", "-m", THEIRS, "HEAD"],
    );
    git(&teammate, &["push", "-q", "origin", "refs/notes/inkentry"]);

    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            mine.to_str().unwrap(),
        ],
    );
    git(&mine, &["config", "user.email", "mine@example.com"]);
    git(&mine, &["config", "user.name", "Mine"]);
    run_init(&mine);

    const MINE: &str = r#"{"schema_version":1,"id":2,"kind":"decision","title":"mine unpushed"}"#;
    git(
        &mine,
        &["notes", "--ref=inkentry", "add", "-f", "-m", MINE, "HEAD"],
    );

    git(&mine, &["fetch", "-q", "origin"]);

    let after = git_stdout(&mine, &["notes", "--ref=inkentry", "show", "HEAD"]);
    assert!(
        after.contains("mine unpushed"),
        "a plain fetch must not clobber a local unpushed note, got:\n{after}"
    );
    assert!(
        git_out(&mine, &["rev-parse", "--verify", TRACKING_REF])
            .status
            .success(),
        "the teammate's note should land on {TRACKING_REF}"
    );
}
