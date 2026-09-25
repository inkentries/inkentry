use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use assert_cmd::Command;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const TRACKING_REF: &str = "refs/notes/origin/inkentry";

fn git(dir: &Path, args: &[&str]) {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> std::process::Output {
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
    String::from_utf8_lossy(&git_out(dir, args).stdout).into_owned()
}

// Explicit, so each test controls when a fetch happens.
fn fetch_notes(dir: &Path) {
    git(
        dir,
        &[
            "fetch",
            "-q",
            "origin",
            &format!("refs/notes/inkentry:{TRACKING_REF}"),
        ],
    );
}

fn init_repo_with_commit(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("f.txt"), "x\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn bin(home: &Path, cwd: &Path) -> Command {
    let mut cmd = inkentry_bin_in(home);
    cmd.current_dir(cwd)
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL");
    cmd
}

fn write_config(dir: &Path, contents: &str) -> PathBuf {
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, contents).unwrap();
    cfg
}

fn empty_config(dir: &Path) -> PathBuf {
    write_config(dir, "")
}

// `dir` doubles as HOME so the import writes `dir/.inkentry/memory.db`.
fn run_init(dir: &Path) -> String {
    let cfg = empty_config(dir);
    let out = inkentry_bin_in(dir)
        .current_dir(dir)
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

fn memory_add(home: &Path, dir: &Path, title: &str, extra: &[&str]) {
    let mut cmd = bin(home, dir);
    cmd.args([
        "memory",
        "add",
        "--kind",
        "decision",
        "--title",
        title,
        "--body",
        &format!("body of {title}"),
    ]);
    cmd.args(extra);
    let out = cmd.output().expect("spawn inkentry memory add");
    assert!(
        out.status.success(),
        "memory add {title:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn memory_list(home: &Path, dir: &Path) -> Vec<serde_json::Value> {
    let out = bin(home, dir)
        .args([
            "memory",
            "list",
            "--archived",
            "--format",
            "jsonl",
            "--limit",
            "100",
        ])
        .output()
        .expect("spawn inkentry memory list");
    assert!(
        out.status.success(),
        "memory list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

fn str_field(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|f| f.as_str())
        .unwrap_or_else(|| panic!("no string field {key:?} in {v:#?}"))
        .to_string()
}

// Clones number entities independently, so cross-clone comparisons go by title.
fn local_id_for_title(home: &Path, dir: &Path, title: &str) -> String {
    let entries = memory_list(home, dir);
    entries
        .iter()
        .find(|v| v.get("title").and_then(|t| t.as_str()) == Some(title))
        .map(|v| str_field(v, "id"))
        .unwrap_or_else(|| panic!("no local entry titled {title:?} in {entries:#?}"))
}

fn edge_triples(home: &Path, dir: &Path) -> Vec<(String, String, String)> {
    let entries = memory_list(home, dir);
    let title_of: std::collections::HashMap<String, String> = entries
        .iter()
        .map(|v| (str_field(v, "id"), str_field(v, "title")))
        .collect();

    let mut rows: Vec<(String, String, String)> = Vec::new();
    for entry in &entries {
        let id = str_field(entry, "id");
        let out = bin(home, dir)
            .args(["memory", "graph", &id, "--format", "json"])
            .output()
            .expect("spawn inkentry memory graph");
        assert!(
            out.status.success(),
            "memory graph {id} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let graph: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("memory graph emits JSON");
        for edge in graph["outgoing"].as_array().into_iter().flatten() {
            let name = |key: &str| {
                let id = str_field(edge, key);
                title_of.get(&id).cloned().unwrap_or(id)
            };
            rows.push((name("from_id"), str_field(edge, "kind"), name("to_id")));
        }
    }
    rows.sort();
    rows.dedup();
    rows
}

fn triples_of_kinds(
    rows: &[(String, String, String)],
    kinds: &[&str],
) -> Vec<(String, String, String)> {
    rows.iter()
        .filter(|(_, kind, _)| kinds.contains(&kind.as_str()))
        .cloned()
        .collect()
}

// Raw, unfolded records: folding by `entity_id` would hide duplicate appends.
fn carrier_records(dir: &Path) -> Vec<serde_json::Value> {
    let listing = git_stdout(dir, &["notes", "--ref=inkentry", "list"]);
    let mut out = Vec::new();
    for line in listing.lines() {
        let Some(blob) = line.split_whitespace().next() else {
            continue;
        };
        for l in git_stdout(dir, &["cat-file", "-p", blob]).lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(l.trim())
                && v.get("entity_id").is_some()
            {
                out.push(v);
            }
        }
    }
    out
}

fn carried_edges_for_title(dir: &Path, title: &str) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = carrier_records(dir)
        .iter()
        .filter(|r| r.get("title").and_then(|t| t.as_str()) == Some(title))
        .filter_map(|r| r.get("edges").and_then(|e| e.as_array()).cloned())
        .flatten()
        .map(|e| (str_field(&e, "kind"), str_field(&e, "to_entity_id")))
        .collect();
    found.sort();
    found
}

fn entity_id_of_title(dir: &Path, title: &str) -> String {
    let records = carrier_records(dir);
    records
        .iter()
        .find(|r| r.get("title").and_then(|t| t.as_str()) == Some(title))
        .map(|r| str_field(r, "entity_id"))
        .unwrap_or_else(|| panic!("no carrier record titled {title:?} in {records:#?}"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn dump(body: &[&str], counts: &str) -> String {
    let header = r#"{"record":"header","format":"portable-dump","format_version":1,"generated_at":1786370293,"generator":"test/1.0.0"}"#;
    let mut lines = vec![header.to_string()];
    lines.extend(body.iter().map(|s| s.to_string()));

    let mut fold = Sha256::new();
    for line in &lines {
        fold.update(hex(&Sha256::digest(line.as_bytes())).as_bytes());
    }
    let digest = format!("sha256:{}", hex(&fold.finalize()));
    lines.push(format!(
        r#"{{"record":"footer","counts":{counts},"digest":"{digest}"}}"#
    ));
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

// `contradicts` is server-generated; a dump is the only way to seed one without a server.
fn contradiction_dump(from: &str, to: &str) -> String {
    let entry = |dump_ref: &str, title: &str, created_at: i64| {
        format!(
            r#"{{"record":"entity","type":"memory_entry","ref":"{dump_ref}","kind":"decision","title":"{title}","body":"body of {title}","created_at":{created_at}}}"#
        )
    };
    dump(
        &[
            &entry("d1", from, 3000),
            &entry("d2", to, 3100),
            r#"{"record":"relationship","type":"contradicts","from":"d1","to":"d2","created_at":3200}"#,
        ],
        r#"{"entity":{"memory_entry":2},"relationship":{"contradicts":1}}"#,
    )
}

fn run_import(home: &Path, dir: &Path, contents: &str) {
    let path = dir.join("project.dump");
    std::fs::write(&path, contents).unwrap();
    let cfg = empty_config(dir);
    let out = bin(home, dir)
        .arg("--config")
        .arg(&cfg)
        .arg("import")
        .arg(&path)
        .arg("--db")
        .arg(dir.join(".inkentry").join("memory.db"))
        .arg("--no-embed")
        .output()
        .expect("spawn inkentry import");
    assert!(
        out.status.success(),
        "inkentry import failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// Both clones get `.inkentry/` so `memory add` takes the SQLite-plus-carrier path, not the pre-init fallback.
fn setup_origin_with_two_clones(tmp: &Path) -> (PathBuf, PathBuf) {
    let origin = tmp.join("origin.git");
    git(
        tmp,
        &[
            "init",
            "--bare",
            "-q",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );

    let a = tmp.join("a");
    std::fs::create_dir_all(&a).unwrap();
    init_repo_with_commit(&a);
    git(&a, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&a, &["push", "-q", "-u", "origin", "main"]);
    std::fs::create_dir_all(a.join(".inkentry")).unwrap();

    let b = tmp.join("b");
    git(
        tmp,
        &["clone", "-q", origin.to_str().unwrap(), b.to_str().unwrap()],
    );
    git(&b, &["config", "user.email", "b@example.com"]);
    git(&b, &["config", "user.name", "B"]);
    std::fs::create_dir_all(b.join(".inkentry")).unwrap();

    (a, b)
}

fn push_notes(dir: &Path) {
    git(dir, &["push", "-q", "origin", "refs/notes/inkentry"]);
}

// B adopts A's first push, then diverges, so the later notes merge must union two histories rather than fast-forward.
#[test]
fn two_clone_round_trip_reconstructs_both_carried_kinds_despite_a_divergent_note() {
    let tmp = TempDir::new().unwrap();
    let home_a = TempDir::new().unwrap();
    let (a, b) = setup_origin_with_two_clones(tmp.path());

    memory_add(home_a.path(), &a, "the first claim", &[]);
    memory_add(home_a.path(), &a, "the old plan", &[]);
    push_notes(&a);

    fetch_notes(&b);
    run_init(&b);
    assert!(
        memory_list(&b, &b)
            .iter()
            .any(|v| str_field(v, "title") == "the first claim"),
        "setup: B must hold A's first push before diverging"
    );

    memory_add(&b, &b, "clone b local only", &[]);

    let first_claim = local_id_for_title(home_a.path(), &a, "the first claim");
    memory_add(
        home_a.path(),
        &a,
        "the second claim",
        &["--relates-to", &first_claim],
    );
    run_import(
        home_a.path(),
        &a,
        &contradiction_dump("the counterclaim", "the rebuttal"),
    );
    let old_plan = local_id_for_title(home_a.path(), &a, "the old plan");
    memory_add(
        home_a.path(),
        &a,
        "the new plan",
        &["--supersedes", &old_plan],
    );
    push_notes(&a);

    fetch_notes(&b);
    run_init(&b);

    let carried = ["relates_to", "contradicts"];
    let a_edges = triples_of_kinds(&edge_triples(home_a.path(), &a), &carried);
    let b_edges = triples_of_kinds(&edge_triples(&b, &b), &carried);
    assert_eq!(
        a_edges,
        vec![
            (
                "the counterclaim".to_string(),
                "contradicts".to_string(),
                "the rebuttal".to_string()
            ),
            (
                "the second claim".to_string(),
                "relates_to".to_string(),
                "the first claim".to_string()
            ),
        ],
        "setup: A must hold one edge of each carried kind"
    );
    assert_eq!(
        b_edges, a_edges,
        "the clone's graph must equal the writer's for both carried kinds"
    );

    assert!(
        memory_list(&b, &b)
            .iter()
            .any(|v| str_field(v, "title") == "clone b local only"),
        "the notes merge must not drop B's own entry"
    );

    for record in carrier_records(&b) {
        let kinds: Vec<String> = record
            .get("edges")
            .and_then(|e| e.as_array())
            .into_iter()
            .flatten()
            .map(|e| str_field(e, "kind"))
            .collect();
        assert!(
            !kinds.iter().any(|k| k == "supersedes"),
            "supersedes must never ride the edge list, got {record:#?}"
        );
    }
    assert_eq!(
        carried_edges_for_title(&b, "the old plan"),
        vec![],
        "the superseded entry carries its successor on its own field, not as an edge"
    );
    assert!(
        carrier_records(&b).iter().any(|r| {
            r.get("title").and_then(|t| t.as_str()) == Some("the old plan")
                && r.get("superseded_by_entity_id").is_some()
        }),
        "the supersede must have travelled on `superseded_by_entity_id`"
    );

    run_init(&b);
    assert_eq!(
        triples_of_kinds(&edge_triples(&b, &b), &carried),
        b_edges,
        "a second import must not duplicate an edge"
    );
}

#[test]
fn memory_add_relates_to_carries_the_edge_on_the_new_record() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo_with_commit(&repo);
    std::fs::create_dir_all(repo.join(".inkentry")).unwrap();

    memory_add(home.path(), &repo, "the target", &[]);
    let target = local_id_for_title(home.path(), &repo, "the target");
    memory_add(home.path(), &repo, "the source", &["--relates-to", &target]);

    let target_entity_id = entity_id_of_title(&repo, "the target");
    assert_eq!(
        carried_edges_for_title(&repo, "the source"),
        vec![("relates_to".to_string(), target_entity_id)],
        "the edge rides the source entry's record, keyed by the target's entity id"
    );
    assert_eq!(
        carried_edges_for_title(&repo, "the target"),
        vec![],
        "the target's record must not carry a second copy of the same edge"
    );
}

#[test]
fn an_edge_whose_target_is_absent_on_the_clone_is_skipped_and_reported() {
    let tmp = TempDir::new().unwrap();
    let home_a = TempDir::new().unwrap();
    let (a, b) = setup_origin_with_two_clones(tmp.path());

    // Carrier off: the target exists in A's store but never reaches the ref.
    let off = write_config(&a, "store_in_git_notes = false\n");
    let out = bin(home_a.path(), &a)
        .arg("--config")
        .arg(&off)
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "the unshared target",
            "--body",
            "body of the unshared target",
        ])
        .output()
        .expect("spawn inkentry memory add");
    assert!(
        out.status.success(),
        "memory add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let target = local_id_for_title(home_a.path(), &a, "the unshared target");
    memory_add(home_a.path(), &a, "the source", &["--relates-to", &target]);
    assert_eq!(
        carried_edges_for_title(&a, "the source").len(),
        1,
        "setup: the source's record must carry the edge even though its target does not travel"
    );
    push_notes(&a);

    fetch_notes(&b);
    let stdout = run_init(&b);

    assert!(
        stdout.contains("1 edge skipped"),
        "the unresolved edge must be reported, got:\n{stdout}"
    );
    assert!(
        edge_triples(&b, &b).is_empty(),
        "an edge with an absent endpoint must leave no row behind"
    );
    assert!(
        memory_list(&b, &b)
            .iter()
            .all(|v| str_field(v, "title") != "the unshared target"),
        "setup: the target must not have travelled"
    );
}
