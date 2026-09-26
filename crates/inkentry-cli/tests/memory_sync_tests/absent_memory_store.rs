// Push refuses an absent store (exit 2, no file created): sending from nothing is not an
// empty delta. Pull and `sync` create it, which is how a fresh checkout first receives team
// memory. Sync runs a push leg internally, so push's refusal must not be reachable from it.

use crate::plumbing_helpers;
use plumbing_helpers::{
    inkentry_bin_in, mount_memory_batch, mount_memory_since, mount_team_health,
    register_sqlite_vec, write_team_config,
};

use std::path::{Path, PathBuf};
use tempfile::TempDir;
use wiremock::MockServer;

use inkentry_core::storage::MemoryStore;

// A fresh checkout: `.inkentry/config.toml` is committed, `memory.db` is not.
struct Checkout {
    home: TempDir,
    proj: TempDir,
    config_path: PathBuf,
}

impl Checkout {
    fn fresh(server_url: &str) -> Self {
        let home = TempDir::new().unwrap();
        let proj = TempDir::new().unwrap();
        let config_path = write_team_config(proj.path(), server_url);
        let me = Self {
            home,
            proj,
            config_path,
        };
        assert!(
            !me.mem_path().exists(),
            "the fixture is only meaningful without a memory.db"
        );
        me
    }

    fn mem_path(&self) -> PathBuf {
        self.proj.path().join(".inkentry").join("memory.db")
    }

    fn cmd(&self) -> assert_cmd::Command {
        let mut c = inkentry_bin_in(self.home.path());
        c.current_dir(self.proj.path())
            .arg("--config")
            .arg(&self.config_path);
        c
    }
}

fn report(stdout: &[u8]) -> serde_json::Value {
    serde_json::from_slice(stdout).unwrap_or_else(|e| {
        panic!(
            "a plumbing report must be one JSON object on stdout: {e}; stdout={:?}",
            String::from_utf8_lossy(stdout)
        )
    })
}

fn titles_in(mem_path: &Path) -> Vec<String> {
    register_sqlite_vec();
    let store = MemoryStore::open(mem_path).expect("open memory.db");
    let mut titles: Vec<String> = store
        .list(None, 100, true)
        .expect("list notes")
        .into_iter()
        .map(|n| n.title)
        .collect();
    titles.sort();
    titles
}

fn create_empty_store(mem_path: &Path) {
    register_sqlite_vec();
    MemoryStore::open(mem_path).expect("create empty memory.db");
    assert!(mem_path.exists(), "the store file must be on disk");
}

async fn mount_batch_ok(server: &MockServer) {
    mount_memory_batch(
        server,
        serde_json::json!({"created": 1, "skipped": 0, "failed": 0, "results": []}),
    )
    .await;
}

async fn mount_since_empty(server: &MockServer) {
    mount_memory_since(server, serde_json::json!({"entries": []})).await;
}

async fn mount_since_one(server: &MockServer, title: &str) {
    mount_memory_since(
        server,
        serde_json::json!({"entries": [{
            "id": "01890000-0000-7000-8000-0000000000aa",
            "kind": "decision",
            "title": title,
            "body": "written by a teammate",
            "created_at": "2026-06-19T01:00:00Z",
        }]}),
    )
    .await;
}

#[tokio::test]
async fn push_refuses_an_absent_store_and_creates_nothing() {
    let server = MockServer::start().await;
    mount_team_health(&server).await;
    mount_batch_ok(&server).await;

    let checkout = Checkout::fresh(&server.uri());
    let out = checkout
        .cmd()
        .args(["plumbing", "push"])
        .output()
        .expect("run plumbing push");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !checkout.mem_path().exists(),
        "a refused push must not leave a store behind at {}",
        checkout.mem_path().display()
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "an absent store is a hard error, not an empty delta; stderr={stderr}"
    );
    assert!(
        out.stdout.is_empty(),
        "exit 2 leaves stdout empty; stdout={:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        stderr.contains("No memory store found"),
        "the diagnostic must name the absent store; stderr={stderr}"
    );
}

#[tokio::test]
async fn push_refuses_an_absent_explicit_source() {
    let server = MockServer::start().await;
    mount_team_health(&server).await;
    mount_batch_ok(&server).await;

    let checkout = Checkout::fresh(&server.uri());
    let source = checkout.proj.path().join("nowhere").join("memory.db");
    let out = checkout
        .cmd()
        .args(["plumbing", "push", "--source"])
        .arg(&source)
        .output()
        .expect("run plumbing push --source");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !source.exists(),
        "a refused push must not create the source it was pointed at, {}",
        source.display()
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "a named source that is not there is a hard error; stderr={stderr}"
    );
    assert!(
        stderr.contains(&source.display().to_string()),
        "the diagnostic must name the path the caller gave; stderr={stderr}"
    );
}

#[tokio::test]
async fn pull_creates_an_absent_store_and_applies_entries() {
    let server = MockServer::start().await;
    mount_team_health(&server).await;
    mount_since_one(&server, "teammate entry").await;

    let checkout = Checkout::fresh(&server.uri());
    let out = checkout
        .cmd()
        .args(["plumbing", "pull"])
        .output()
        .expect("run plumbing pull");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let report = report(&out.stdout);
    assert!(
        checkout.mem_path().exists(),
        "pull must create the store it receives into; stderr={stderr}"
    );
    assert_eq!(
        titles_in(&checkout.mem_path()),
        vec!["teammate entry".to_string()],
        "the pulled entry must land in the new store; report={report}, stderr={stderr}"
    );
    assert_eq!(report["applied"], 1, "report={report}; stderr={stderr}");
    assert_eq!(
        out.status.code(),
        Some(0),
        "an entry was applied; stderr={stderr}"
    );
}

// Guards the bootstrap: sync's push leg must not inherit push's refusal.
#[tokio::test]
async fn sync_on_a_fresh_checkout_with_no_store_still_works() {
    let server = MockServer::start().await;
    mount_team_health(&server).await;
    mount_batch_ok(&server).await;
    mount_since_empty(&server).await;

    let checkout = Checkout::fresh(&server.uri());
    let out = checkout.cmd().arg("sync").output().expect("run sync");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !stderr.contains("No memory store found"),
        "push's refusal must not reach sync's push leg; stderr={stderr}"
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "sync bootstraps a fresh checkout; stderr={stderr}"
    );
    assert!(
        checkout.mem_path().exists(),
        "sync must create the store it converges into; stderr={stderr}"
    );
}

// With `cloud_first` and a `server_url` the team server owns the store, so an absent
// local file says nothing and neither transfer may refuse on it.
#[tokio::test]
async fn cloud_first_push_does_not_refuse_an_absent_store() {
    let server = MockServer::start().await;
    mount_team_health(&server).await;
    mount_batch_ok(&server).await;

    let checkout = Checkout::fresh(&server.uri());
    let out = checkout
        .cmd()
        .env("INKENTRY_MODE", "cloud_first")
        .args(["plumbing", "push"])
        .output()
        .expect("run plumbing push");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_ne!(
        out.status.code(),
        Some(2),
        "pull creates in every mode, cloud_first included; stderr={stderr}"
    );
    let report = report(&out.stdout);
    assert_eq!(report["attempted"], 0, "report={report}; stderr={stderr}");
    assert_eq!(
        out.status.code(),
        Some(1),
        "nothing local to push is an empty delta; stderr={stderr}"
    );
}

#[tokio::test]
async fn cloud_first_pull_still_applies_without_a_local_store() {
    let server = MockServer::start().await;
    mount_team_health(&server).await;
    mount_since_one(&server, "teammate entry").await;

    let checkout = Checkout::fresh(&server.uri());
    let out = checkout
        .cmd()
        .env("INKENTRY_MODE", "cloud_first")
        .args(["plumbing", "pull"])
        .output()
        .expect("run plumbing pull");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_ne!(
        out.status.code(),
        Some(2),
        "the cloud_first carve-out must hold; stderr={stderr}"
    );
    assert_eq!(report(&out.stdout)["applied"], 1, "stderr={stderr}");
}

// A present-but-empty store must stay distinguishable from an absent one: exit 1 with
// the report on stdout.
#[tokio::test]
async fn an_existing_empty_store_is_still_an_empty_delta_for_both() {
    let server = MockServer::start().await;
    mount_team_health(&server).await;
    mount_batch_ok(&server).await;
    mount_since_empty(&server).await;

    let checkout = Checkout::fresh(&server.uri());
    create_empty_store(&checkout.mem_path());

    let pushed = checkout
        .cmd()
        .args(["plumbing", "push"])
        .output()
        .expect("run plumbing push");
    let push_stderr = String::from_utf8_lossy(&pushed.stderr).into_owned();
    let push_report = report(&pushed.stdout);
    assert_eq!(
        pushed.status.code(),
        Some(1),
        "an empty store is an empty delta, not a refusal; stderr={push_stderr}"
    );
    assert_eq!(push_report["attempted"], 0, "report={push_report}");
    assert_eq!(push_report["created"], 0, "report={push_report}");
    assert_eq!(push_report["already_synced"], 0, "report={push_report}");

    let pulled = checkout
        .cmd()
        .args(["plumbing", "pull"])
        .output()
        .expect("run plumbing pull");
    let pull_stderr = String::from_utf8_lossy(&pulled.stderr).into_owned();
    let pull_report = report(&pulled.stdout);
    assert_eq!(
        pulled.status.code(),
        Some(1),
        "nothing new to apply is an empty delta; stderr={pull_stderr}"
    );
    assert_eq!(pull_report["applied"], 0, "report={pull_report}");
}
