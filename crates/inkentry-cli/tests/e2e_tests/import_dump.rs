use crate::plumbing_helpers;
use plumbing_helpers::{inkentry_bin_in, parse_jsonl, write_config};

use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// Footer digest: SHA-256 over the concatenated lowercase-hex per-record SHA-256s,
// header included, footer excluded.
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

fn entry(dump_ref: &str, title: &str, created_at: i64, extra: &str) -> String {
    format!(
        r#"{{"record":"entity","type":"memory_entry","ref":"{dump_ref}","kind":"decision","title":"{title}","body":"body of {title}","created_at":{created_at}{extra}}}"#
    )
}

struct Project {
    _tmp: TempDir,
    // One home and registry for every command in a test: import writes the registry
    // and assertions read it back.
    home: TempDir,
    mem_path: std::path::PathBuf,
    config_path: std::path::PathBuf,
    root: std::path::PathBuf,
    // Isolated so a developer's own server on the default port 4655 is never the
    // embedder under test.
    state_dir: std::path::PathBuf,
    discovery_port: std::cell::RefCell<String>,
}

fn project() -> Project {
    let p = project_that_may_find_an_embedder();
    // Offline, so a running loopback embedder is never auto-discovered.
    let mut cfg = std::fs::read_to_string(&p.config_path).unwrap();
    cfg.push_str("mode = \"offline\"\n");
    std::fs::write(&p.config_path, cfg).unwrap();
    p
}

fn project_that_may_find_an_embedder() -> Project {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let mem_path = db_path.with_file_name("memory.db");
    // Port 1 is never listening, so no embedder is reachable.
    let config_path = write_config(tmp.path(), &db_path, "http://127.0.0.1:1");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let root = tmp.path().to_path_buf();
    Project {
        _tmp: tmp,
        home,
        mem_path,
        config_path,
        root,
        state_dir,
        discovery_port: std::cell::RefCell::new("0".to_string()),
    }
}

impl Project {
    fn bin(&self) -> assert_cmd::Command {
        let mut cmd = inkentry_bin_in(self.home.path());
        cmd.env("INKENTRY_REGISTRY_DIR", self.home.path())
            .env("INKENTRY_STATE_DIR", &self.state_dir)
            .env(
                "INKENTRY_TEST_DISCOVERY_PORT",
                self.discovery_port.borrow().as_str(),
            );
        cmd
    }

    // Points loopback discovery's fixed-port fallback at a mock embedder; `server.port`
    // is honoured only for a live inkentry-server pid.
    fn set_embedder(&self, uri: &str) {
        let port = uri.rsplit(':').next().unwrap().trim_end_matches('/');
        *self.discovery_port.borrow_mut() = port.to_string();
    }

    fn import(&self, contents: &str) -> assert_cmd::Command {
        let path = self.root.join("project.dump");
        std::fs::write(&path, contents).unwrap();
        let mut cmd = self.bin();
        cmd.current_dir(&self.root)
            .arg("--config")
            .arg(&self.config_path)
            .arg("import")
            .arg(&path)
            .arg("--db")
            .arg(&self.mem_path);
        cmd
    }

    fn entries(&self) -> Vec<serde_json::Value> {
        let out = self
            .bin()
            .current_dir(&self.root)
            .arg("--config")
            .arg(&self.config_path)
            .arg("memory")
            .arg("--db")
            .arg(&self.mem_path)
            .arg("list")
            .arg("--archived")
            .arg("--format")
            .arg("jsonl")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        parse_jsonl(&out)
    }

    fn sql<T: rusqlite::types::FromSql>(&self, query: &str) -> T {
        let conn = rusqlite::Connection::open(&self.mem_path).unwrap();
        conn.query_row(query, [], |r| r.get(0)).unwrap()
    }

    fn registered_projects(&self) -> i64 {
        let path = self.home.path().join("registry.db");
        if !path.exists() {
            return 0;
        }
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row("SELECT count(*) FROM projects", [], |r| r.get(0))
            .unwrap_or(0)
    }
}

#[test]
fn entries_and_their_relationship_land_together() {
    let p = project();
    let d = dump(
        &[
            &entry("e1", "old", 1000, r#","status":"archived""#),
            &entry("e2", "new", 2000, ""),
            r#"{"record":"relationship","type":"supersedes","from":"e2","to":"e1","created_at":2500}"#,
        ],
        r#"{"entity":{"memory_entry":2},"relationship":{"supersedes":1}}"#,
    );
    p.import(&d).assert().success();

    let rows = p.entries();
    assert_eq!(rows.len(), 2);
    assert_eq!(p.sql::<i64>("SELECT count(*) FROM memory_edges"), 1);
}

#[test]
fn the_report_names_the_entity_id_as_the_portable_one() {
    let p = project();
    let d = dump(
        &[&entry("e1", "carried", 1000, "")],
        r#"{"entity":{"memory_entry":1},"relationship":{}}"#,
    );
    let out = p.import(&d).assert().success();
    let report = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(report.contains("Imported 1 memory entry"), "{report}");
    assert!(report.contains("minted on this machine"), "{report}");
    assert!(report.contains("entity id"), "{report}");
}

#[test]
fn an_entry_arriving_with_an_identity_keeps_it_verbatim() {
    let p = project();
    let carried = "0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33";
    let d = dump(
        &[&entry(
            "e1",
            "carried",
            1000,
            &format!(r#","uuid":"{carried}""#),
        )],
        r#"{"entity":{"memory_entry":1},"relationship":{}}"#,
    );
    p.import(&d).assert().success();

    let rows = p.entries();
    assert_eq!(rows[0]["id"].as_str(), Some(carried));
}

#[test]
fn an_entry_arriving_without_one_is_identified_from_its_own_creation_time() {
    let p = project();
    // Dump lists newest-first; identifiers must follow created_at, not import order.
    let d = dump(
        &[
            &entry("e3", "newest", 3_000_000_000, ""),
            &entry("e1", "oldest", 1_000_000_000, ""),
            &entry("e2", "middle", 2_000_000_000, ""),
        ],
        r#"{"entity":{"memory_entry":3},"relationship":{}}"#,
    );
    p.import(&d).assert().success();

    let conn = rusqlite::Connection::open(&p.mem_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT title FROM notes ORDER BY uuid")
        .unwrap();
    let by_identifier: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        by_identifier,
        vec!["oldest", "middle", "newest"],
        "assigned identifiers must sort in creation order, not import order"
    );
}

#[test]
fn a_relationship_before_its_entities_imports_the_same_way() {
    let p = project();
    let d = dump(
        &[
            r#"{"record":"relationship","type":"relates_to","from":"e2","to":"e1"}"#,
            &entry("e1", "first", 1000, ""),
            &entry("e2", "second", 2000, ""),
        ],
        r#"{"entity":{"memory_entry":2},"relationship":{"relates_to":1}}"#,
    );
    p.import(&d).assert().success();
    assert_eq!(p.sql::<i64>("SELECT count(*) FROM memory_edges"), 1);
}

#[test]
fn the_supersede_column_is_set_from_the_relationship_in_the_right_direction() {
    let p = project();
    let d = dump(
        &[
            &entry("e1", "predecessor", 1000, r#","status":"archived""#),
            &entry("e2", "successor", 2000, ""),
            r#"{"record":"relationship","type":"supersedes","from":"e2","to":"e1"}"#,
        ],
        r#"{"entity":{"memory_entry":2},"relationship":{"supersedes":1}}"#,
    );
    p.import(&d).assert().success();

    // `from` is the successor, so the PREDECESSOR carries the link; easy to get backwards.
    let pointing: String = p.sql("SELECT n.title FROM notes n WHERE n.superseded_by IS NOT NULL");
    assert_eq!(pointing, "predecessor");
    let target: String = p.sql(
        "SELECT t.title FROM notes n JOIN notes t ON t.uuid = n.superseded_by \
         WHERE n.superseded_by IS NOT NULL",
    );
    assert_eq!(target, "successor");
}

#[test]
fn the_same_supersede_fact_twice_yields_one_edge_and_one_column_value() {
    let p = project();
    let rel = r#"{"record":"relationship","type":"supersedes","from":"e2","to":"e1"}"#;
    let d = dump(
        &[
            &entry("e1", "predecessor", 1000, r#","status":"archived""#),
            &entry("e2", "successor", 2000, ""),
            rel,
            rel,
        ],
        r#"{"entity":{"memory_entry":2},"relationship":{"supersedes":2}}"#,
    );
    // Check the reported count: `INSERT OR IGNORE` would absorb a duplicate silently,
    // so row counts alone cannot show the reader deduplicated.
    let out = p
        .import(&d)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let summary: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        summary["memory_edges"].as_u64(),
        Some(1),
        "the same fact twice is one relationship, not two applied twice"
    );
    assert_eq!(summary["supersede_links"].as_u64(), Some(1));

    assert_eq!(p.sql::<i64>("SELECT count(*) FROM memory_edges"), 1);
    assert_eq!(
        p.sql::<i64>("SELECT count(*) FROM notes WHERE superseded_by IS NOT NULL"),
        1
    );
}

#[test]
fn an_entry_with_no_supersede_relationship_has_no_supersede_link() {
    let p = project();
    let d = dump(
        &[&entry("e1", "alone", 1000, "")],
        r#"{"entity":{"memory_entry":1},"relationship":{}}"#,
    );
    p.import(&d).assert().success();
    assert_eq!(
        p.sql::<i64>("SELECT count(*) FROM notes WHERE superseded_by IS NOT NULL"),
        0
    );
}

#[test]
fn an_altered_dump_is_refused_and_nothing_is_written() {
    let p = project();
    let good = dump(
        &[&entry("e1", "one", 1000, ""), &entry("e2", "two", 2000, "")],
        r#"{"entity":{"memory_entry":2},"relationship":{}}"#,
    );
    let tampered = good.replace("body of two", "body of TWO");
    assert_ne!(good, tampered);

    p.import(&tampered)
        .assert()
        .failure()
        .stderr(predicates::str::contains("digest"));

    assert!(
        !p.mem_path.exists() || p.sql::<i64>("SELECT count(*) FROM notes") == 0,
        "a refused import must leave nothing behind"
    );
}

#[test]
fn a_relationship_endpoint_that_does_not_resolve_refuses_the_whole_dump() {
    let p = project();
    let d = dump(
        &[
            &entry("e1", "one", 1000, ""),
            r#"{"record":"relationship","type":"relates_to","from":"e1","to":"ghost"}"#,
        ],
        r#"{"entity":{"memory_entry":1},"relationship":{"relates_to":1}}"#,
    );
    p.import(&d).assert().failure();
    assert!(
        !p.mem_path.exists() || p.sql::<i64>("SELECT count(*) FROM notes") == 0,
        "the entity must not be imported when a relationship cannot resolve"
    );
}

#[test]
fn an_unrecognised_record_kind_is_refused_not_skipped() {
    let p = project();
    let d = dump(
        &[
            &entry("e1", "one", 1000, ""),
            r#"{"record":"annotation","text":"something new"}"#,
        ],
        r#"{"entity":{"memory_entry":1},"relationship":{}}"#,
    );
    p.import(&d).assert().failure();
    assert!(
        !p.mem_path.exists() || p.sql::<i64>("SELECT count(*) FROM notes") == 0,
        "an unknown record kind refuses the file rather than importing the rest"
    );
}

#[test]
fn the_git_notes_import_cursor_is_not_carried_across() {
    let p = project();
    let d = dump(
        &[&entry("e1", "one", 1000, "")],
        r#"{"entity":{"memory_entry":1},"relationship":{}}"#,
    );
    p.import(&d).assert().success();

    // The cursor is keyed on notes-ref OIDs that a rename invalidates; carrying it
    // would suppress the first git-notes import.
    assert_eq!(
        p.sql::<i64>("SELECT count(*) FROM notes_import_state"),
        0,
        "the import cursor must start empty, not arrive with the dump"
    );
}

#[test]
fn an_import_with_no_embedder_still_succeeds_and_says_what_is_left() {
    let p = project();
    let d = dump(
        &[&entry("e1", "one", 1000, ""), &entry("e2", "two", 2000, "")],
        r#"{"entity":{"memory_entry":2},"relationship":{}}"#,
    );
    p.import(&d)
        .assert()
        .success()
        .stderr(predicates::str::contains("semantic search"))
        .stderr(predicates::str::contains("inkentry memory reindex"));
}

#[test]
fn status_reports_the_entries_still_waiting_to_be_embedded() {
    let p = project();
    p.bin()
        .current_dir(&p.root)
        .arg("--config")
        .arg(&p.config_path)
        .arg("init")
        .arg("--no-index")
        .assert()
        .success();
    let project_mem = p.root.join(".inkentry").join("memory.db");

    let d = dump(
        &[&entry("e1", "one", 1000, ""), &entry("e2", "two", 2000, "")],
        r#"{"entity":{"memory_entry":2},"relationship":{}}"#,
    );
    let dump_path = p.root.join("project.dump");
    std::fs::write(&dump_path, &d).unwrap();
    p.bin()
        .current_dir(&p.root)
        .arg("--config")
        .arg(&p.config_path)
        .arg("import")
        .arg(&dump_path)
        .arg("--db")
        .arg(&project_mem)
        .assert()
        .success();

    let out = p
        .bin()
        .current_dir(&p.root)
        .arg("--config")
        .arg(&p.config_path)
        .arg("status")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        status["memory_embedding_pending"].as_u64(),
        Some(2),
        "status must surface the pending count, not leave it to be discovered"
    );
}

// Same kind/title/body from different commits share one convergence key
// (`entity_id` is UNIQUE) and collapse to one row; the count must describe what landed.
#[test]
fn two_entries_that_collapse_into_one_are_counted_as_one() {
    let p = project();
    let same = |dump_ref: &str, source_ref: &str| {
        format!(
            r#"{{"record":"entity","type":"memory_entry","ref":"{dump_ref}","kind":"decision","title":"Retry with backoff","body":"same text","created_at":1000,"source_ref":"{source_ref}"}}"#
        )
    };
    let d = dump(
        &[&same("e1", "commit:aaa"), &same("e2", "commit:bbb")],
        r#"{"entity":{"memory_entry":2},"relationship":{}}"#,
    );
    let out = p
        .import(&d)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let summary: serde_json::Value = serde_json::from_slice(&out).unwrap();

    assert_eq!(
        p.sql::<i64>("SELECT count(*) FROM notes"),
        1,
        "one convergence key is one row"
    );
    assert_eq!(
        summary["memory_entries"].as_u64(),
        Some(1),
        "the reported count must be the number of entries that landed: {summary}"
    );
    assert_eq!(
        summary["memory_entries_merged"].as_u64(),
        Some(1),
        "and the one that folded into it must be reported, not dropped in silence: {summary}"
    );
}

// Survivor is the earliest-created, not the first emitted: dump record order is unconstrained.
#[test]
fn the_surviving_entry_is_the_earliest_created_one_whatever_the_dump_order() {
    let survivor_of = |body_order: [(&str, i64, &str); 2]| {
        let p = project();
        let lines: Vec<String> = body_order
            .iter()
            .map(|(dump_ref, created_at, tag)| {
                format!(
                    r#"{{"record":"entity","type":"memory_entry","ref":"{dump_ref}","kind":"decision","title":"One","body":"same text","created_at":{created_at},"tags":["{tag}"]}}"#
                )
            })
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let d = dump(&refs, r#"{"entity":{"memory_entry":2},"relationship":{}}"#);
        p.import(&d).assert().success();
        p.sql::<i64>("SELECT created_at FROM notes")
    };

    assert_eq!(survivor_of([("e1", 1000, "a"), ("e2", 2000, "b")]), 1000);
    assert_eq!(
        survivor_of([("e2", 2000, "b"), ("e1", 1000, "a")]),
        1000,
        "reversing the dump order must not change which entry survives"
    );
}

#[test]
fn a_collapsed_entrys_tags_are_folded_into_the_survivor() {
    let p = project();
    let tagged = |dump_ref: &str, created_at: i64, tag: &str| {
        format!(
            r#"{{"record":"entity","type":"memory_entry","ref":"{dump_ref}","kind":"decision","title":"One","body":"same text","created_at":{created_at},"tags":["{tag}"]}}"#
        )
    };
    let d = dump(
        &[&tagged("e1", 1000, "keep"), &tagged("e2", 2000, "alsokeep")],
        r#"{"entity":{"memory_entry":2},"relationship":{}}"#,
    );
    p.import(&d).assert().success();
    // One survivor row, so every row in `note_tags` belongs to it.
    let tags: String = p.sql("SELECT GROUP_CONCAT(tag) FROM note_tags");
    assert!(tags.contains("keep") && tags.contains("alsokeep"), "{tags}");
}

#[test]
fn re_importing_the_same_dump_reports_that_nothing_new_landed() {
    let p = project();
    let d = dump(
        &[&entry("e1", "one", 1000, ""), &entry("e2", "two", 2000, "")],
        r#"{"entity":{"memory_entry":2},"relationship":{}}"#,
    );
    p.import(&d).assert().success();

    let out = p
        .import(&d)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let summary: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(p.sql::<i64>("SELECT count(*) FROM notes"), 2);
    assert_eq!(
        summary["memory_entries"].as_u64(),
        Some(0),
        "nothing was added the second time: {summary}"
    );
    assert_eq!(
        summary["memory_entries_already_present"].as_u64(),
        Some(2),
        "and the entries that were already there must be named as such: {summary}"
    );
}

// Projects go to the registry and commands to index.db, outside the memory
// transaction; a refusal must leave all three stores untouched.
#[test]
fn a_refused_dump_leaves_the_registry_untouched_too() {
    let p = project();
    let before = p.registered_projects();
    let d = dump(
        &[
            r#"{"record":"entity","type":"project","ref":"p1","root_path":"/tmp/imported-alpha"}"#,
            &entry("e1", "one", 1000, ""),
            // relates_to between a project and a memory entry cannot be stored.
            r#"{"record":"relationship","type":"relates_to","from":"p1","to":"e1"}"#,
        ],
        r#"{"entity":{"memory_entry":1,"project":1},"relationship":{"relates_to":1}}"#,
    );
    p.import(&d).assert().failure();

    assert!(
        !p.mem_path.exists() || p.sql::<i64>("SELECT count(*) FROM notes") == 0,
        "no memory entry may survive a refusal"
    );
    assert_eq!(
        p.registered_projects(),
        before,
        "and no project may survive it either"
    );
}

#[test]
fn two_entities_sharing_a_uuid_are_refused_with_a_message_about_the_dump() {
    let p = project();
    let uuid = "0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33";
    let d = dump(
        &[
            &entry("e1", "one", 1000, &format!(r#","uuid":"{uuid}""#)),
            &entry("e2", "two", 2000, &format!(r#","uuid":"{uuid}""#)),
        ],
        r#"{"entity":{"memory_entry":2},"relationship":{}}"#,
    );
    p.import(&d)
        .assert()
        .failure()
        .stderr(predicates::str::contains("share the identity"))
        .stderr(predicates::str::contains(uuid));
}

#[test]
fn two_entities_sharing_a_remote_id_are_refused_with_a_message_about_the_dump() {
    let p = project();
    let d = dump(
        &[
            &entry("e1", "one", 1000, r#","remote_id":"rem-9""#),
            &entry("e2", "two", 2000, r#","remote_id":"rem-9""#),
        ],
        r#"{"entity":{"memory_entry":2},"relationship":{}}"#,
    );
    p.import(&d)
        .assert()
        .failure()
        .stderr(predicates::str::contains("rem-9"))
        .stderr(predicates::str::contains("Refusing to import any of it"));
}

// Probes clap rather than pinning the message string, which could name a
// nonexistent command.
#[test]
fn the_command_the_legacy_refusal_names_is_one_this_binary_accepts() {
    let p = project();
    let legacy = p.root.join("legacy-memory.db");
    {
        let conn = rusqlite::Connection::open(&legacy).unwrap();
        conn.execute_batch(
            "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT); PRAGMA user_version = 10;",
        )
        .unwrap();
    }

    let out = p
        .bin()
        .current_dir(&p.root)
        .arg("--config")
        .arg(&p.config_path)
        .arg("memory")
        .arg("--db")
        .arg(&legacy)
        .arg("list")
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let message = String::from_utf8_lossy(&out).into_owned();

    // Check every backticked `inkentry ...` span; `spelunk-export` in the message is not a subcommand.
    let named: Vec<Vec<&str>> = message
        .split('`')
        .skip(1)
        .step_by(2)
        .filter_map(|span| span.strip_prefix("inkentry "))
        .map(|sub| sub.split_whitespace().collect())
        .filter(|sub: &Vec<&str>| !sub.is_empty())
        .collect();
    assert!(
        !named.is_empty(),
        "the refusal must name an inkentry command in backticks: {message}"
    );

    for subcommand in named {
        let probe = p
            .bin()
            .args(&subcommand)
            .arg("--help")
            .assert()
            .get_output()
            .stderr
            .clone();
        let probe = String::from_utf8_lossy(&probe).into_owned();
        assert!(
            !probe.contains("unrecognized subcommand") && !probe.contains("unexpected argument"),
            "the refusal sends the user to `inkentry {}`, which this binary does not accept: {probe}",
            subcommand.join(" ")
        );
    }
}

#[test]
fn an_entry_carrying_a_blank_identity_is_refused_by_name() {
    for (field, value) in [
        ("uuid", ""),
        ("uuid", "   "),
        ("entity_id", ""),
        ("remote_id", ""),
    ] {
        let p = project();
        let d = dump(
            &[&entry(
                "e1",
                "blank",
                1000,
                &format!(r#","{field}":"{value}""#),
            )],
            r#"{"entity":{"memory_entry":1},"relationship":{}}"#,
        );
        p.import(&d)
            .assert()
            .failure()
            .stderr(predicates::str::contains(format!("blank {field}")))
            .stderr(predicates::str::contains("\"e1\""))
            .stderr(predicates::str::contains("\"blank\""));

        assert!(
            !p.mem_path.exists() || p.sql::<i64>("SELECT count(*) FROM notes") == 0,
            "a blank {field} must be refused before anything is written"
        );
    }
}

#[test]
fn a_project_whose_memory_lives_on_a_server_refuses_the_import() {
    let p = project();
    // `server_url` is honoured only from the project config or env, not `--config`.
    let inkentry_dir = p.root.join(".inkentry");
    std::fs::create_dir_all(&inkentry_dir).unwrap();
    std::fs::write(
        inkentry_dir.join("config.toml"),
        "server_url = \"http://127.0.0.1:1\"\nproject_id = \"team/proj\"\n",
    )
    .unwrap();
    let cfg = std::fs::read_to_string(&p.config_path)
        .unwrap()
        .replace("mode = \"offline\"", "mode = \"cloud_first\"");
    std::fs::write(&p.config_path, cfg).unwrap();

    let d = dump(
        &[&entry("e1", "one", 1000, "")],
        r#"{"entity":{"memory_entry":1},"relationship":{}}"#,
    );
    p.import(&d)
        .assert()
        .failure()
        .stderr(predicates::str::contains("cloud_first"))
        .stderr(predicates::str::contains("http://127.0.0.1:1"));

    assert!(
        !p.mem_path.exists(),
        "the refusal must come before a local store is created"
    );
}

fn mock_embedder() -> (tokio::runtime::Runtime, wiremock::MockServer) {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(async {
        let server = MockServer::start().await;
        plumbing_helpers::mount_health(&server).await;
        // Wire format: raw little-endian f32, one 896-dim vector per chunk.
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/octet-stream")
                    .set_body_bytes(
                        (0..896)
                            .flat_map(|_| 0.01f32.to_le_bytes())
                            .collect::<Vec<u8>>(),
                    ),
            )
            .mount(&server)
            .await;
        server
    });
    (rt, server)
}

fn embedded_entries(mem_path: &std::path::Path) -> i64 {
    plumbing_helpers::register_sqlite_vec();
    let conn = rusqlite::Connection::open(mem_path).unwrap();
    conn.query_row("SELECT count(*) FROM note_embeddings", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_json_import_that_reaches_an_embedder_writes_one_document_to_stdout() {
    let p = project_that_may_find_an_embedder();
    let (_rt, server) = mock_embedder();
    p.set_embedder(&server.uri());

    let d = dump(
        &[&entry("e1", "one", 1000, ""), &entry("e2", "two", 2000, "")],
        r#"{"entity":{"memory_entry":2},"relationship":{}}"#,
    );
    let out = p
        .import(&d)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    // The finishing pass must not emit its own json summary: a second document
    // would break `from_slice`.
    let summary: serde_json::Value = serde_json::from_slice(&out).unwrap_or_else(|e| {
        panic!(
            "stdout must be exactly one json document ({e}): {}",
            String::from_utf8_lossy(&out)
        )
    });
    assert_eq!(summary["memory_entries"], 2);

    // Without this the test proves nothing: an unreachable embedder returns before
    // the finishing pass prints.
    assert_eq!(
        embedded_entries(&p.mem_path),
        2,
        "the embedder must actually have been reached"
    );
}
