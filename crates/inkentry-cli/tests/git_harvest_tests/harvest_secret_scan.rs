// Secret scanning on the git-commit harvest (`inkentry harvest --source git`).
//
// A commit message is already in shared git history, so harvest does not leak
// anything new by reading one. What it does is *promote* that text into
// memory, which is written to `refs/notes/inkentry` and pushed to a team or
// hosted server, the same destination `memory add` refuses to write a
// matched secret to. This path therefore applies the same scanner.
//
// The failure shape is deliberately not `memory add`'s. `add` aborts the
// whole command, which is right for one interactive title/body; a `--branch`
// walk can cover thousands of commits, so one match skips that commit and the
// walk continues. The tests below pin that difference: a match must not end
// the run, and the warning must name the commit SHA without echoing what
// matched.

use crate::plumbing_helpers;
use plumbing_helpers::{inkentry_bin_in, isolate_git_config};

use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// An AWS access key ID, split so this file's own literal does not trip the
// scanner that indexes this repository.
fn aws_key() -> String {
    format!("AKIA{}", "IOSFODNN7EXAMPLE")
}

// ── mock inference server ─────────────────────────────────────────────────

// Embedding vectors are derived from the request's chunk content rather than
// being a constant: harvest drops an entry whose nearest neighbour is closer
// than 0.15, so identical vectors for every entry would make each commit after
// the first look like a duplicate and hide whether the walk really stored it.
struct ContentDerivedEmbedResponder;

impl wiremock::Respond for ContentDerivedEmbedResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        #[derive(serde::Deserialize)]
        struct Chunk {
            content: String,
        }
        #[derive(serde::Deserialize)]
        struct ReqBody {
            chunks: Vec<Chunk>,
        }

        let body: ReqBody =
            serde_json::from_slice(&request.body).unwrap_or(ReqBody { chunks: vec![] });

        let dim = inkentry_core::embeddings::EMBEDDING_DIM;
        let mut bytes = Vec::with_capacity(body.chunks.len() * dim * 4);
        for chunk in &body.chunks {
            // One-hot on a content-derived axis: distinct texts land on
            // orthogonal vectors (cosine distance 1.0), identical texts on the
            // same one, so the dedup check still means something.
            let axis = (content_axis(&chunk.content)) % dim;
            for i in 0..dim {
                let v: f32 = if i == axis { 1.0 } else { 0.0 };
                bytes.extend_from_slice(&v.to_le_bytes());
            }
        }

        ResponseTemplate::new(200)
            .insert_header("content-type", "application/octet-stream")
            .set_body_bytes(bytes)
    }
}

fn content_axis(text: &str) -> usize {
    let mut acc: usize = 0;
    for (i, b) in text.bytes().enumerate() {
        acc = acc
            .wrapping_mul(31)
            .wrapping_add(b as usize)
            .wrapping_add(i);
    }
    acc
}

// The extraction reply is built from the prompt the CLI actually sent: one
// entry per `COMMIT <sha>` line. A commit that never reaches the LLM therefore
// yields no entry, so "stored" in these tests tracks "was sent for extraction"
// exactly.
//
// The prompt carries each commit's subject on the line after its sha, and the
// reply echoes it as the title. Naming the sha instead would make every entry
// the same sentence bar a hex string, which embeds close enough that harvest's
// dedup pass drops one as a duplicate of another, on nothing but which shas a
// run happened to generate. Real subjects keep the entries apart.
fn extraction_reply(prompt: &str) -> String {
    let lines: Vec<&str> = prompt.lines().collect();
    let entries: Vec<serde_json::Value> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let sha = line.strip_prefix("COMMIT ")?.trim();
            let subject = lines.get(i + 1)?.trim();
            Some((sha, subject))
        })
        .map(|(sha, subject)| {
            serde_json::json!({
                "sha": sha[..sha.len().min(8)].to_string(),
                "kind": "decision",
                "title": subject,
                "body": format!("{subject}. Extracted from commit {sha} by the mock extractor."),
                "tags": ["fixture"],
            })
        })
        .collect();
    serde_json::json!({ "entries": entries }).to_string()
}

async fn inference_mock() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory", "index.embed", "search.semantic", "llm.complete"],
            "embedding_dim": inkentry_core::embeddings::EMBEDDING_DIM,
            "embedder": { "state": "ready", "detail": null },
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(ContentDerivedEmbedResponder)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/llm/complete$"))
        .respond_with(|req: &wiremock::Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);
            let prompt = body["messages"]
                .as_array()
                .and_then(|m| m.last())
                .and_then(|m| m["content"].as_str())
                .unwrap_or("")
                .to_string();
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(plumbing_helpers::sse_token_response(&extraction_reply(
                    &prompt,
                )))
        })
        .mount(&server)
        .await;
    server
}

// Every `/llm/complete` prompt this mock received, concatenated.
async fn llm_prompts(server: &MockServer) -> String {
    server
        .received_requests()
        .await
        .expect("requests recorded")
        .iter()
        .filter(|r| r.url.path().ends_with("/llm/complete"))
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

// ── repo + project fixtures ───────────────────────────────────────────────

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn head_sha(dir: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .expect("git rev-parse");
    assert!(out.status.success(), "git rev-parse HEAD failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// A git project with one indexable source file, ready for further commits.
fn init_project(dir: &Path) {
    isolate_git_config();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("create src dir");
    std::fs::write(
        src.join("lib.rs"),
        "pub fn greet(name: &str) -> String {\n    format!(\"hello, {name}\")\n}\n",
    )
    .expect("write lib.rs");
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["add", "."]);
    git(
        dir,
        &[
            "commit",
            "-q",
            "-m",
            "feat: choose sqlite over postgres for the local index",
        ],
    );
}

// Add an empty commit carrying `subject` (and `body`, when non-empty), and
// return its full SHA.
fn commit(dir: &Path, subject: &str, body: &str) -> String {
    let mut args = vec!["commit", "--allow-empty", "-q", "-m", subject];
    if !body.is_empty() {
        args.push("-m");
        args.push(body);
    }
    git(dir, &args);
    head_sha(dir)
}

fn base_cmd(home: &Path, project: &Path) -> assert_cmd::Command {
    let mut cmd = inkentry_bin_in(home);
    cmd.current_dir(project)
        .env_remove("INKENTRY_SERVER_URL")
        .env_remove("INKENTRY_MODE")
        .env_remove("INKENTRY_PROJECT_ID")
        .env_remove("INKENTRY_NO_SERVER")
        .env_remove("INKENTRY_STATE_DIR")
        .env_remove("INKENTRY_LLM_URL")
        .env_remove("INKENTRY_LLM_MODEL");
    cmd
}

// Point loopback auto-discovery at `url` so harvest resolves both inference
// routes to the mock without a team `server_url`.
fn write_loopback_state(state_dir: &Path, url: &str) {
    std::fs::create_dir_all(state_dir).expect("create state dir");
    let port: u16 = url
        .rsplit(':')
        .next()
        .expect("uri has a port")
        .trim_end_matches('/')
        .parse()
        .expect("uri port is numeric");
    std::fs::write(state_dir.join("server.port"), format!("{port}\n")).expect("write server.port");
}

// `inkentry index` offline, so the project has the `.inkentry/` directory
// harvest requires (ADR-067) without contacting anything.
fn seed_index(home: &Path, project: &Path, db: &Path) {
    base_cmd(home, project)
        .env("INKENTRY_NO_SERVER", "1")
        .arg("index")
        .arg("--db")
        .arg(db)
        .arg("--no-summaries")
        .arg(".")
        .assert()
        .success();
}

fn combined(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// The `source_ref` of every note in `mem_db`.
fn stored_source_refs(mem_db: &Path) -> Vec<String> {
    plumbing_helpers::register_sqlite_vec();
    if !mem_db.exists() {
        return vec![];
    }
    let conn = rusqlite::Connection::open(mem_db).expect("open memory db");
    let mut stmt = conn
        .prepare("SELECT source_ref FROM notes WHERE source_ref IS NOT NULL")
        .expect("prepare source_ref query");
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query source_refs");
    rows.map(|r| r.expect("read source_ref")).collect()
}

struct Harness {
    _home: TempDir,
    project: TempDir,
    state_dir: std::path::PathBuf,
    mem_db: std::path::PathBuf,
    home_path: std::path::PathBuf,
}

async fn harness() -> (Harness, MockServer) {
    let server = inference_mock().await;
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    init_project(project.path());
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &server.uri());
    let home_path = home.path().to_path_buf();
    let mem_db = project.path().join("memory.db");
    (
        Harness {
            _home: home,
            project,
            state_dir,
            mem_db,
            home_path,
        },
        server,
    )
}

impl Harness {
    fn harvest(&self, extra: &[&str]) -> std::process::Output {
        let mut cmd = base_cmd(&self.home_path, self.project.path());
        cmd.env("INKENTRY_STATE_DIR", &self.state_dir)
            .arg("harvest")
            .arg("--db")
            .arg(&self.mem_db)
            .arg("--branch")
            .arg("HEAD");
        for a in extra {
            cmd.arg(a);
        }
        cmd.output().expect("run harvest")
    }
}

// ── tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn harvest_skips_a_commit_whose_message_matches_a_secret_pattern() {
    let (h, server) = harness().await;
    let index_db = h.project.path().join("index.db");
    seed_index(&h.home_path, h.project.path(), &index_db);

    let key = aws_key();
    let secret_sha = commit(
        h.project.path(),
        "fix: point the deploy job at the new bucket",
        &format!("Rotated after the outage; old value was {key}."),
    );
    let clean_sha = commit(
        h.project.path(),
        "feat: add a retry budget so flaky upstreams cannot stall the queue",
        "",
    );

    let output = h.harvest(&[]);
    let text = combined(&output);

    assert!(
        output.status.success(),
        "one matching commit must not fail the harvest:\n{text}"
    );

    assert!(
        !text.contains("[dedup]"),
        "no commit here is a restatement of another, so a dedup drop means the \
         fixture built entries that read alike rather than the walk losing a \
         commit; the missing-sha assertion below would report that as the \
         wrong defect:\n{text}"
    );

    let refs = stored_source_refs(&h.mem_db);
    assert!(
        !refs.contains(&secret_sha),
        "the matching commit must not be stored, got {refs:?}:\n{text}"
    );
    assert!(
        refs.contains(&clean_sha),
        "the walk must continue past the match and store {clean_sha}, got {refs:?}:\n{text}"
    );

    assert!(
        text.contains(&secret_sha),
        "the warning must name the skipped commit:\n{text}"
    );
    assert!(
        !text.contains(&key),
        "the warning must never echo what matched:\n{text}"
    );
    assert!(
        !llm_prompts(&server).await.contains(&key),
        "a matched commit message must not be sent for extraction"
    );
}

#[tokio::test]
async fn a_branch_walk_stores_every_clean_commit_despite_one_match() {
    let (h, server) = harness().await;
    let index_db = h.project.path().join("index.db");
    seed_index(&h.home_path, h.project.path(), &index_db);

    let key = aws_key();
    let mut clean = Vec::new();
    for subject in [
        "feat: cache resolved refs so the walker stops re-reading packfiles",
        "refactor: split the parser so grammar upgrades stay local",
    ] {
        clean.push(commit(h.project.path(), subject, ""));
    }
    let secret_sha = commit(
        h.project.path(),
        "chore: replace the leaked CI token",
        &format!("The exposed value was {key}, now revoked."),
    );
    for subject in [
        "fix: give the queue a bounded channel so a slow consumer cannot OOM the host",
        "feat: record why the embedding dimension is pinned",
    ] {
        clean.push(commit(h.project.path(), subject, ""));
    }

    // Batch size 2 puts the matching commit in the middle of a multi-batch
    // walk: a skip that ended the run would leave later batches unharvested.
    let output = h.harvest(&["--batch-size", "2"]);
    let text = combined(&output);

    assert!(
        output.status.success(),
        "a multi-batch walk must survive one matching commit:\n{text}"
    );

    let refs = stored_source_refs(&h.mem_db);
    for sha in &clean {
        assert!(
            refs.contains(sha),
            "clean commit {sha} must still be stored, got {refs:?}:\n{text}"
        );
    }
    assert!(
        !refs.contains(&secret_sha),
        "the matching commit must not be stored, got {refs:?}:\n{text}"
    );
    assert!(
        !llm_prompts(&server).await.contains(&key),
        "a matched commit message must not be sent for extraction"
    );
}
