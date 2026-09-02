// `memory add`'s embed step: the entry must be durable before the embed is
// attempted, and the wait for the vector must be bounded.
//
// The embedder these tests point auto-discovery at is a mock, so a "stalled"
// embedder is a mounted response delay rather than a real bulk index batch.
// That is the same shape the real stall has from the CLI's side: a request
// that has been accepted and is not coming back soon.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::MemoryAddArgs;
use super::sync::test_support::{
    LoopbackEmbedder, mount_health, point_discovery_at, register_sqlite_vec, stub_vector,
};
use crate::config::Config;
use crate::storage::MemoryStore;

const PROJECT: &str = "proj";

fn embed_route() -> String {
    format!("/v1/projects/{PROJECT}/index/embed")
}

fn cfg_for(_project_root: &std::path::Path) -> Config {
    Config {
        project_id: Some(PROJECT.to_string()),
        ..Default::default()
    }
}

fn add_args(title: &str, body: &str) -> MemoryAddArgs {
    MemoryAddArgs {
        title: Some(title.to_string()),
        body: Some(body.to_string()),
        from_url: None,
        kind: "note".to_string(),
        tags: None,
        files: None,
        valid_at: None,
        supersedes: None,
        relates_to: None,
    }
}

// A temp project with an initialised `memory.db`, returning the tempdir (kept
// alive by the caller) and the store path `memory add` writes to.
//
// It is a real git repository with a commit, because the write-through carrier
// needs a HEAD to attach its record to. Without one the carrier write fails and
// the ordering test can only speak for SQLite.
fn fresh_project() -> (tempfile::TempDir, PathBuf) {
    register_sqlite_vec();
    let tmp = tempfile::TempDir::new().unwrap();
    git(tmp.path(), &["init", "--quiet", "."]);
    git(tmp.path(), &["config", "user.email", "test@example.com"]);
    git(tmp.path(), &["config", "user.name", "test"]);
    git(
        tmp.path(),
        &["commit", "--quiet", "--allow-empty", "-m", "root"],
    );
    let mem_path = tmp.path().join("memory.db");
    // Created and closed here so the file exists with its schema before the
    // command under test opens it.
    drop(MemoryStore::open(&mem_path).unwrap());
    (tmp, mem_path)
}

fn git(root: &std::path::Path, args: &[&str]) {
    let out = inkentry_core::test_support::git_command(root)
        .args(args)
        .output()
        .expect("git should be available");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// The write-through carrier's record for `title`, as git notes holds it.
fn carrier_holds(root: &std::path::Path, title: &str) -> bool {
    inkentry_core::test_support::git_command(root)
        .args(["notes", "--ref=inkentry", "show", "HEAD"])
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains(title))
}

fn stored_titles(mem_path: &std::path::Path) -> Vec<String> {
    MemoryStore::open(mem_path)
        .unwrap()
        .rows_for_sync(true)
        .unwrap()
        .into_iter()
        .map(|r| r.title)
        .collect()
}

fn embedding_for(mem_path: &std::path::Path, title: &str) -> Option<Vec<u8>> {
    let store = MemoryStore::open(mem_path).unwrap();
    let rows = store.rows_for_sync(true).unwrap();
    let row = rows
        .iter()
        .find(|r| r.title == title)
        .expect("entry stored");
    store.get_embedding(&row.id).unwrap()
}

// ── 1. Durable before the embed request goes out ────────────────────────────

// Answers the embed, but first records whether the entry was already readable
// from `memory.db` at the moment the request arrived. This is the ordering
// assertion: a store-after-embed implementation cannot make this true.
struct RecordStoreStateOnArrival {
    project_root: PathBuf,
    mem_path: PathBuf,
    title: String,
    entry_present_on_arrival: Arc<AtomicBool>,
    carrier_present_on_arrival: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
}

impl wiremock::Respond for RecordStoreStateOnArrival {
    fn respond(&self, _req: &wiremock::Request) -> ResponseTemplate {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let present = MemoryStore::open(&self.mem_path)
            .ok()
            .and_then(|s| s.rows_for_sync(true).ok())
            .is_some_and(|rows| rows.iter().any(|r| r.title == self.title));
        self.entry_present_on_arrival
            .store(present, Ordering::SeqCst);
        self.carrier_present_on_arrival.store(
            carrier_holds(&self.project_root, &self.title),
            Ordering::SeqCst,
        );
        ResponseTemplate::new(200)
            .set_body_bytes(inkentry_core::embeddings::vec_to_blob(&stub_vector()))
    }
}

#[tokio::test]
#[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
async fn the_entry_is_stored_before_the_embed_request_is_sent() {
    let (tmp, mem_path) = fresh_project();
    let title = "Durable before embedding";

    let present = Arc::new(AtomicBool::new(false));
    let carried = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(AtomicUsize::new(0));
    let server = MockServer::start().await;
    mount_health(&server).await;
    Mock::given(method("POST"))
        .and(path(embed_route()))
        .respond_with(RecordStoreStateOnArrival {
            project_root: tmp.path().to_path_buf(),
            mem_path: mem_path.clone(),
            title: title.to_string(),
            entry_present_on_arrival: Arc::clone(&present),
            carrier_present_on_arrival: Arc::clone(&carried),
            requests: Arc::clone(&requests),
        })
        .mount(&server)
        .await;
    let _embedder: LoopbackEmbedder = point_discovery_at(server);

    super::add::memory_add(
        add_args(
            title,
            "the entry must be readable before its vector is asked for",
        ),
        &mem_path,
        &cfg_for(tmp.path()),
        None,
        false,
    )
    .await
    .expect("memory add should succeed");

    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "the add path should have sent exactly one embed request"
    );
    assert!(
        present.load(Ordering::SeqCst),
        "the entry must already be readable from memory.db when the embed request \
         arrives, so a lost or stalled embed cannot lose the entry"
    );
    assert!(
        carried.load(Ordering::SeqCst),
        "the write-through carrier record must also already be written when the \
         embed request arrives"
    );
}

// ── 2. A stalled embedder is bounded, not fatal ─────────────────────────────

#[tokio::test]
#[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
async fn a_stalled_embedder_defers_the_vector_within_the_budget() {
    let (tmp, mem_path) = fresh_project();
    let title = "Stored while the embedder is busy";

    // Far longer than the interactive budget: this stands in for the embedder
    // being held by a bulk index batch, where the measured wait ran to minutes.
    let stall = super::add::INTERACTIVE_EMBED_BUDGET * 20;
    let server = MockServer::start().await;
    mount_health(&server).await;
    Mock::given(method("POST"))
        .and(path(embed_route()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(stall)
                .set_body_bytes(inkentry_core::embeddings::vec_to_blob(&stub_vector())),
        )
        .mount(&server)
        .await;
    let _embedder = point_discovery_at(server);

    let started = std::time::Instant::now();
    super::add::memory_add(
        add_args(title, "a busy embedder must not cost the caller the entry"),
        &mem_path,
        &cfg_for(tmp.path()),
        None,
        false,
    )
    .await
    .expect("a stalled embedder must not fail the command");
    let elapsed = started.elapsed();

    assert!(
        elapsed < stall,
        "memory add waited {elapsed:?}, which means it sat out the whole stall \
         instead of giving up on the vector"
    );
    assert!(
        stored_titles(&mem_path).contains(&title.to_string()),
        "the entry must be stored even though its vector never arrived"
    );
    assert!(
        embedding_for(&mem_path, title).is_none(),
        "the entry should be left vectorless for the catch-up paths, not given a \
         partial or placeholder vector"
    );
}

// The wording the deferral prints has one job: name the command that finishes
// the work. Asserted on the message itself because the command writes it to
// stderr, which an in-process test cannot capture.
#[test]
fn the_deferral_warning_names_the_command_that_embeds_the_entry() {
    let warning = super::add::pending_embedding_warning("embedding it timed out");
    assert!(
        warning.contains("inkentry memory reindex"),
        "the warning must name `inkentry memory reindex`, got: {warning}"
    );
    assert!(
        warning.contains("embedding it timed out"),
        "the warning must say why the vector is missing, got: {warning}"
    );
}

// ── 3. The catch-up paths pick the entry up ─────────────────────────────────

#[tokio::test]
#[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
async fn memory_reindex_attaches_the_vector_a_deferred_add_left_missing() {
    let (tmp, mem_path) = fresh_project();
    let title = "Deferred then reindexed";
    let body = "the catch-up path must mint the vector the add did not";

    // Add with the embedder stalled: the entry lands vectorless.
    let stalled = MockServer::start().await;
    mount_health(&stalled).await;
    Mock::given(method("POST"))
        .and(path(embed_route()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(super::add::INTERACTIVE_EMBED_BUDGET * 20)
                .set_body_bytes(inkentry_core::embeddings::vec_to_blob(&stub_vector())),
        )
        .mount(&stalled)
        .await;
    let stalled_embedder = point_discovery_at(stalled);
    super::add::memory_add(
        add_args(title, body),
        &mem_path,
        &cfg_for(tmp.path()),
        None,
        false,
    )
    .await
    .expect("memory add should succeed");
    drop(stalled_embedder);
    assert!(
        embedding_for(&mem_path, title).is_none(),
        "precondition: the add left the entry vectorless"
    );

    // A responsive embedder, and the ordinary backfill command.
    let healthy = MockServer::start().await;
    mount_health(&healthy).await;
    Mock::given(method("POST"))
        .and(path(embed_route()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(inkentry_core::embeddings::vec_to_blob(&stub_vector())),
        )
        .mount(&healthy)
        .await;
    let _healthy_embedder = point_discovery_at(healthy);

    super::reindex::memory_reindex(
        super::MemoryReindexArgs {
            dry_run: false,
            force: false,
            include_archived: false,
            format: "text".to_string(),
        },
        &mem_path,
        &cfg_for(tmp.path()),
        None,
        super::reindex::Summary::Suppressed,
    )
    .await
    .expect("memory reindex should succeed");

    let vector = embedding_for(&mem_path, title).expect("reindex should attach the vector");
    assert_eq!(
        inkentry_core::embeddings::blob_to_vec(&vector).len(),
        inkentry_core::embeddings::EMBEDDING_DIM,
        "the backfilled vector must be a full-dimension embedding"
    );
}

// An add-time vector and a reindex-time one must be the same bytes for the same
// entry, or the two paths rank the same entry against different spaces.
#[tokio::test]
#[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
async fn an_add_time_vector_and_a_reindexed_one_are_identical() {
    use super::sync::test_support::{content_vector, spawn_content_embedder};

    let title = "Same entry, same vector";
    let body = "add time and reindex time must agree on the bytes";

    let (tmp, mem_path) = fresh_project();
    let _embedder = spawn_content_embedder(PROJECT, None).await;
    super::add::memory_add(
        add_args(title, body),
        &mem_path,
        &cfg_for(tmp.path()),
        None,
        false,
    )
    .await
    .expect("memory add should succeed");
    let add_time = embedding_for(&mem_path, title).expect("the fast path attaches a vector");

    // The document string both paths embed, derived here rather than read back
    // from either implementation.
    let expected = inkentry_core::embeddings::vec_to_blob(&content_vector(&format!(
        "title: {title} | text: {body}"
    )));
    assert_eq!(
        add_time, expected,
        "the add-time vector must be the embedding of `title: … | text: …`"
    );

    super::reindex::memory_reindex(
        super::MemoryReindexArgs {
            dry_run: false,
            force: true,
            include_archived: false,
            format: "text".to_string(),
        },
        &mem_path,
        &cfg_for(tmp.path()),
        None,
        super::reindex::Summary::Suppressed,
    )
    .await
    .expect("memory reindex should succeed");

    assert_eq!(
        embedding_for(&mem_path, title).expect("still embedded after reindex"),
        add_time,
        "re-embedding the same entry must reproduce the add-time vector exactly"
    );
}

// ── 4. The fast path is unchanged ───────────────────────────────────────────

#[tokio::test]
#[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
async fn a_prompt_embed_leaves_the_entry_embedded() {
    use super::sync::test_support::spawn_loopback_embedder;

    let (tmp, mem_path) = fresh_project();
    let title = "Embedded on the fast path";
    let _embedder = spawn_loopback_embedder(PROJECT, None).await;

    super::add::memory_add(
        add_args(
            title,
            "a responsive embedder should behave exactly as before",
        ),
        &mem_path,
        &cfg_for(tmp.path()),
        None,
        false,
    )
    .await
    .expect("memory add should succeed");

    let vector = embedding_for(&mem_path, title).expect("the fast path attaches a vector");
    assert_eq!(
        inkentry_core::embeddings::blob_to_vec(&vector),
        stub_vector(),
        "the stored vector must be the one the embedder returned"
    );
    assert!(
        MemoryStore::open(&mem_path)
            .unwrap()
            .notes_missing_embeddings(false)
            .unwrap()
            .is_empty(),
        "nothing should be left for the catch-up paths on the fast path"
    );
}

// ── 5. A deferral is not a retry storm ──────────────────────────────────────

#[tokio::test]
#[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
async fn a_timed_out_embed_is_sent_once_and_not_retried() {
    let (tmp, mem_path) = fresh_project();
    let requests = Arc::new(AtomicUsize::new(0));

    struct CountAndStall {
        requests: Arc<AtomicUsize>,
        delay: Duration,
    }
    impl wiremock::Respond for CountAndStall {
        fn respond(&self, _req: &wiremock::Request) -> ResponseTemplate {
            self.requests.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200)
                .set_delay(self.delay)
                .set_body_bytes(inkentry_core::embeddings::vec_to_blob(&stub_vector()))
        }
    }

    let server = MockServer::start().await;
    mount_health(&server).await;
    Mock::given(method("POST"))
        .and(path(embed_route()))
        .respond_with(CountAndStall {
            requests: Arc::clone(&requests),
            delay: super::add::INTERACTIVE_EMBED_BUDGET * 20,
        })
        .mount(&server)
        .await;
    let _embedder = point_discovery_at(server);

    super::add::memory_add(
        add_args(
            "One shot only",
            "giving up must not add load to a busy embedder",
        ),
        &mem_path,
        &cfg_for(tmp.path()),
        None,
        false,
    )
    .await
    .expect("memory add should succeed");

    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "an embed that runs out of budget must be abandoned, not retried against \
         an embedder that is already saturated"
    );
}
