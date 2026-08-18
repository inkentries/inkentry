// The background pass that fills in vectors for rows stored without one, and
// the signals that wake it. Built on the shared `support` harness because the
// pass takes an `AppState`: the same state a request was served from, so a
// degraded write and the sweep that repairs it are the same server.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{self, Request};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::support::{make_state_with_slot, note_item, recording_slot};
use crate::repair::RepairSignal;

// An embedder that fails every call, standing in for a model that is loaded
// and answering but cannot serve this text: the case a readiness-transition
// trigger can never reach, because readiness already happened.
struct FailingEmbedder {
    dim: usize,
}

#[async_trait::async_trait]
impl inkentry_core::embeddings::EmbeddingBackend for FailingEmbedder {
    async fn embed(&self, _texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        anyhow::bail!("embedder refused this text")
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

// Returns one vector fewer than it was asked for. The input-to-output mapping
// is then unknown, so no vector may be assigned to any entry.
struct ShortCountEmbedder {
    dim: usize,
}

#[async_trait::async_trait]
impl inkentry_core::embeddings::EmbeddingBackend for ShortCountEmbedder {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .skip(1)
            .map(|_| vec![0.5_f32; self.dim])
            .collect())
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

async fn post_batch_to(state: &crate::AppState, slug: &str, entries: Value) -> (u16, Value) {
    let body = json!({ "entries": entries });
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/projects/{slug}/memory/batch"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = crate::router(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post_note_to(state: &crate::AppState, slug: &str, title: &str) -> u16 {
    let body = json!({"kind": "note", "title": title, "body": "b"});
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/projects/{slug}/memory"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    crate::router(state.clone())
        .oneshot(req)
        .await
        .unwrap()
        .status()
        .as_u16()
}

// A raise already stored resolves immediately; the absence of one only shows
// up as a wait that does not finish, so it costs a short real delay.
async fn signal_is_pending(signal: &RepairSignal) -> bool {
    tokio::time::timeout(Duration::from_millis(100), signal.wait())
        .await
        .is_ok()
}

// ── Both degrade paths signal ─────────────────────────────────────────

// Since the embed moved off the lock into one batched call, an embed error
// drops the vector for every entry in the request rather than for one. It also
// happens long after the embedder became ready, so no readiness transition
// follows it and the write itself has to ask for repair.
#[tokio::test]
async fn a_batch_whose_embed_fails_stores_every_entry_text_only_and_signals_repair() {
    let state = make_state_with_slot(
        4,
        crate::EmbedderSlot::ready(Arc::new(FailingEmbedder { dim: 4 })),
    );
    let entries: Vec<Value> = (0..50)
        .map(|i| note_item(&format!("t{i}"), &format!("e{i}")))
        .collect();

    let (status, body) = post_batch_to(&state, "failing", json!(entries)).await;
    assert_eq!(
        status, 207,
        "an embed failure must not fail the write: {body}"
    );
    assert_eq!(body["created"], json!(50), "{body}");
    assert_eq!(body["skipped"], json!(0), "{body}");
    assert_eq!(body["failed"], json!(0), "{body}");
    for i in 0..50 {
        assert_eq!(
            body["results"][i]["embedded"],
            json!(false),
            "entry {i} must report that it is not in the index: {body}"
        );
    }
    assert!(
        signal_is_pending(&state.repair_signal).await,
        "storing 50 vectorless rows must ask for repair"
    );
}

#[tokio::test]
async fn a_batch_whose_embed_returns_too_few_vectors_signals_repair() {
    let state = make_state_with_slot(
        4,
        crate::EmbedderSlot::ready(Arc::new(ShortCountEmbedder { dim: 4 })),
    );
    let entries = json!([note_item("a", "e0"), note_item("b", "e1")]);

    let (status, body) = post_batch_to(&state, "shortcount", entries).await;
    assert_eq!(status, 207, "{body}");
    assert_eq!(body["created"], json!(2), "{body}");
    assert_eq!(
        body["results"][0]["embedded"],
        json!(false),
        "an unmappable vector count must assign no vectors at all: {body}"
    );
    assert_eq!(body["results"][1]["embedded"], json!(false), "{body}");
    assert!(signal_is_pending(&state.repair_signal).await);
}

#[tokio::test]
async fn a_batch_stored_while_the_embedder_is_not_ready_signals_repair() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    let (status, _) = post_batch_to(&state, "loading", json!([note_item("t", "e0")])).await;
    assert_eq!(status, 207);
    assert!(signal_is_pending(&state.repair_signal).await);
}

// The single-entry route stores under the identical policy, so it is the same
// hole. Its wire response is out of scope here; the signal is not.
#[tokio::test]
async fn a_single_note_stored_without_a_vector_signals_repair() {
    for slot in [
        crate::EmbedderSlot::loading(),
        crate::EmbedderSlot::ready(Arc::new(FailingEmbedder { dim: 4 })),
    ] {
        let state = make_state_with_slot(4, slot);
        let status = post_note_to(&state, "single", "t").await;
        assert_eq!(status, 201, "the entry is still stored");
        assert!(
            signal_is_pending(&state.repair_signal).await,
            "a single-entry write that stored no vector must ask for repair"
        );
    }
}

// The counterweight: a write that lands in the index must not wake the worker,
// or every healthy push would schedule a pointless scan of the whole store.
#[tokio::test]
async fn a_fully_embedded_batch_raises_no_signal() {
    let (slot, _embedded) = recording_slot(4);
    let state = make_state_with_slot(4, slot);
    let (status, body) = post_batch_to(&state, "healthy", json!([note_item("t", "e0")])).await;
    assert_eq!(status, 207, "{body}");
    assert_eq!(body["results"][0]["embedded"], json!(true), "{body}");
    assert!(
        !signal_is_pending(&state.repair_signal).await,
        "a healthy write must leave the worker asleep"
    );
}

// ── Coalescing and quiescence ─────────────────────────────────────────

#[tokio::test]
async fn signals_coalesce_into_one_wakeup() {
    let signal = RepairSignal::new();
    signal.raise();
    signal.raise();
    signal.raise();

    assert!(
        signal_is_pending(&signal).await,
        "a raise must wake the worker"
    );
    assert!(
        !signal_is_pending(&signal).await,
        "three raises must not queue three sweeps"
    );
}

#[tokio::test]
async fn an_unraised_signal_never_wakes_the_worker() {
    assert!(!signal_is_pending(&RepairSignal::new()).await);
}

// ── HTTP-level status quo the field must not disturb ──────────────────

#[tokio::test]
async fn a_failed_embed_leaves_whole_batch_validation_ordering_alone() {
    let state = make_state_with_slot(
        4,
        crate::EmbedderSlot::ready(Arc::new(FailingEmbedder { dim: 4 })),
    );
    let entries = json!([
        note_item("fine", "e0"),
        {"kind": "note", "title": "x".repeat(crate::handlers::MAX_TITLE_LEN + 1), "external_id": "e1"},
    ]);
    let (status, body) = post_batch_to(&state, "ordering", entries).await;
    assert_eq!(
        status,
        http::StatusCode::BAD_REQUEST.as_u16(),
        "an oversized entry still rejects the whole batch before anything is written: {body}"
    );

    let (_, listed) = post_batch_to(&state, "ordering", json!([note_item("fine", "e0")])).await;
    assert_eq!(
        listed["created"],
        json!(1),
        "the rejected batch must have written nothing, so this is a create: {listed}"
    );
}

// ── The sweep ─────────────────────────────────────────────────────────

// How many active rows in this state have no vector, read straight from the
// store: the only honest way to check, since a stored row's absence from the
// index is invisible to every read route.
async fn vectorless_count(state: &crate::AppState) -> usize {
    let db = state.db.lock().await;
    db.notes_missing_embeddings(0, 10_000)
        .expect("candidate query")
        .len()
}

async fn sweep(state: &crate::AppState) -> crate::repair::RepairStats {
    crate::repair::repair_missing_embeddings(state, 8)
        .await
        .expect("a sweep must not error")
}

// The whole point: a row accepted while nothing could embed it becomes
// searchable once something can, without the client doing anything.
#[tokio::test]
async fn a_sweep_embeds_rows_that_were_stored_without_a_vector() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    let entries: Vec<Value> = (0..5)
        .map(|i| note_item(&format!("t{i}"), &format!("e{i}")))
        .collect();
    post_batch_to(&state, "degraded", json!(entries)).await;
    assert_eq!(vectorless_count(&state).await, 5);

    let (slot, embedded) = recording_slot(4);
    let ready = crate::AppState {
        embedder: slot,
        ..state.clone()
    };
    let stats = sweep(&ready).await;

    assert_eq!(stats.repaired, 5, "{stats:?}");
    assert_eq!(stats.failed, 0, "{stats:?}");
    assert!(!stats.stopped_early, "{stats:?}");
    assert_eq!(vectorless_count(&ready).await, 0);

    let seen = embedded.lock().unwrap();
    assert!(
        seen.contains(&"title: t0 | text: ".to_string()),
        "a repaired row must be embedded from the same text shape the push path \
         uses, or the repaired vector and the pushed vector describe differently \
         shaped strings: {seen:?}"
    );
}

// The harm this exists to close: the row is not merely missing a column, it is
// missing from semantic search entirely until the sweep runs.
#[tokio::test]
async fn a_repaired_row_becomes_reachable_by_the_knn_query() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    post_batch_to(&state, "knn", json!([note_item("findable", "e0")])).await;

    let query = vec![0.5_f32; 4];
    {
        let db = state.db.lock().await;
        let project = db.get_project("knn").expect("project").expect("exists");
        assert!(
            db.search_notes(project.id, &query, 10)
                .expect("knn")
                .is_empty(),
            "a vectorless row must be invisible to KNN, which is the harm"
        );
    }

    let (slot, _) = recording_slot(4);
    let ready = crate::AppState {
        embedder: slot,
        ..state.clone()
    };
    sweep(&ready).await;

    let db = ready.db.lock().await;
    let project = db.get_project("knn").expect("project").expect("exists");
    let hits = db.search_notes(project.id, &query, 10).expect("knn");
    assert_eq!(hits.len(), 1, "the repaired row must now be findable");
    assert_eq!(hits[0].title, "findable");
}

#[tokio::test]
async fn a_second_sweep_finds_nothing_left_to_do() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    post_batch_to(&state, "idem", json!([note_item("t", "e0")])).await;

    let (slot, embedded) = recording_slot(4);
    let ready = crate::AppState {
        embedder: slot,
        ..state.clone()
    };
    assert_eq!(sweep(&ready).await.repaired, 1);

    let second = sweep(&ready).await;
    assert_eq!(second, crate::repair::RepairStats::default(), "{second:?}");
    assert_eq!(
        embedded.lock().unwrap().len(),
        1,
        "an already-repaired row must not be embedded a second time"
    );
}

// Archived rows are excluded everywhere else a note is read back; the sweep
// must not resurrect them into the index.
#[tokio::test]
async fn a_sweep_ignores_archived_rows() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    post_batch_to(&state, "arch", json!([note_item("gone", "e0")])).await;
    {
        let db = state.db.lock().await;
        let project = db.get_project("arch").expect("project").expect("exists");
        let listed = db.list_notes(project.id, None, 10, false).expect("list");
        db.archive_note(project.id, &listed[0].id).expect("archive");
    }

    let (slot, _) = recording_slot(4);
    let ready = crate::AppState {
        embedder: slot,
        ..state.clone()
    };
    let stats = sweep(&ready).await;
    assert_eq!(
        stats,
        crate::repair::RepairStats::default(),
        "an archived row is not a repair candidate: {stats:?}"
    );
}

// One vec0 table at one dimension behind one embedder: a caller's project has
// no bearing on whether a row is repairable.
#[tokio::test]
async fn a_sweep_repairs_every_project_not_just_one() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    post_batch_to(&state, "team-a", json!([note_item("a", "e0")])).await;
    post_batch_to(&state, "team-b", json!([note_item("b", "e0")])).await;

    let (slot, _) = recording_slot(4);
    let ready = crate::AppState {
        embedder: slot,
        ..state.clone()
    };
    assert_eq!(sweep(&ready).await.repaired, 2);
    assert_eq!(vectorless_count(&ready).await, 0);
}

// A backlog larger than one page must still drain, in pages: the alternative
// is one unbounded query holding the global lock over the whole store.
#[tokio::test]
async fn a_sweep_drains_a_backlog_larger_than_one_page() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    let entries: Vec<Value> = (0..20)
        .map(|i| note_item(&format!("t{i}"), &format!("e{i}")))
        .collect();
    post_batch_to(&state, "backlog", json!(entries)).await;

    let (slot, embedded) = recording_slot(4);
    let ready = crate::AppState {
        embedder: slot,
        ..state.clone()
    };
    let stats = crate::repair::repair_missing_embeddings(&ready, 3)
        .await
        .expect("sweep");

    assert_eq!(stats.repaired, 20, "{stats:?}");
    assert_eq!(vectorless_count(&ready).await, 0);
    assert!(
        embedded.lock().unwrap().len() == 20,
        "each row is embedded exactly once across the pages"
    );
}

// `note_embeddings` is a vec0 virtual table with no foreign key, so a row
// deleted between the read and the write would leave a vector addressed to a
// note that no longer exists: an orphan nothing would ever clean up.
#[tokio::test]
async fn a_row_deleted_mid_sweep_acquires_no_orphan_vector() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    post_batch_to(&state, "racy", json!([note_item("doomed", "e0")])).await;

    let (rowid, sync_id, project_id) = {
        let db = state.db.lock().await;
        let project = db.get_project("racy").expect("project").expect("exists");
        let candidates = db.notes_missing_embeddings(0, 10).expect("candidates");
        let listed = db.list_notes(project.id, None, 10, false).expect("list");
        (candidates[0].rowid, listed[0].id.clone(), project.id)
    };

    let db = state.db.lock().await;
    db.delete_note(project_id, &sync_id).expect("delete");
    assert!(
        !db.insert_embedding_if_missing(rowid, &[0.5_f32; 4])
            .expect("write-phase insert"),
        "a row that vanished between the read and the write must not be written"
    );
    assert!(
        db.notes_missing_embeddings(0, 10)
            .expect("candidates")
            .is_empty()
    );
}

// ── Sweep discipline ──────────────────────────────────────────────────

// Nothing the sweep could retry makes a not-ready embedder answer, so it stops
// rather than grinding through a backlog it cannot fix.
#[tokio::test]
async fn a_sweep_with_no_ready_embedder_is_a_quiet_no_op() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    post_batch_to(&state, "cold", json!([note_item("t", "e0")])).await;

    let stats = sweep(&state).await;
    assert!(stats.stopped_early, "{stats:?}");
    assert!(
        !stats.stopped_saturated,
        "a not-ready embedder must not be reported as saturation: the ready \
         transition raises for it, so re-raising here would spin against a \
         model that is still loading; {stats:?}"
    );
    assert_eq!(stats.repaired, 0, "{stats:?}");
    assert_eq!(stats.failed, 0, "{stats:?}");
    assert_eq!(
        vectorless_count(&state).await,
        1,
        "the row is left for a later pass, not consumed"
    );
}

// An embedder that refuses any batch containing one particular text, and
// answers for everything else. This is what a single unembeddable entry looks
// like from the outside: a whole-page failure that says nothing about which
// row caused it.
struct PoisonEmbedder {
    dim: usize,
    poison: &'static str,
}

#[async_trait::async_trait]
impl inkentry_core::embeddings::EmbeddingBackend for PoisonEmbedder {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.iter().any(|t| t.contains(self.poison)) {
            anyhow::bail!("cannot embed this text");
        }
        Ok(texts.iter().map(|_| vec![0.5_f32; self.dim]).collect())
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

// One bad text must cost one vector. Before the retry ladder, a batched embed
// that failed dropped the vector for every entry in the request; if the sweep
// gave up on a page the same way, the poison would keep costing the whole page
// on every pass forever.
#[tokio::test]
async fn a_poisonous_row_costs_only_its_own_vector() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    let entries = json!([
        note_item("healthy-a", "e0"),
        note_item("radioactive", "e1"),
        note_item("healthy-b", "e2"),
    ]);
    post_batch_to(&state, "poison", entries).await;

    let ready = crate::AppState {
        embedder: crate::EmbedderSlot::ready(Arc::new(PoisonEmbedder {
            dim: 4,
            poison: "radioactive",
        })),
        ..state.clone()
    };
    let stats = sweep(&ready).await;

    assert_eq!(
        stats.repaired, 2,
        "the healthy rows are repaired: {stats:?}"
    );
    assert_eq!(stats.failed, 1, "only the poison is left: {stats:?}");
    assert!(!stats.stopped_early, "{stats:?}");

    let left = {
        let db = ready.db.lock().await;
        db.notes_missing_embeddings(0, 10).expect("candidates")
    };
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].title, "radioactive");
}

// Sleeps for the whole embed, so a sweep holding the global lock across it
// would be holding it for a visible stretch of wall clock.
struct SlowEmbedder {
    dim: usize,
}

#[async_trait::async_trait]
impl inkentry_core::embeddings::EmbeddingBackend for SlowEmbedder {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        Ok(texts.iter().map(|_| vec![0.5_f32; self.dim]).collect())
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

// The `ServerDb` lock is global, so a sweep that held it across its embed
// would stall memory reads, the stream poll loop and liveness for the whole
// backlog. Nothing in the type system prevents that, which is why it is
// asserted rather than assumed.
#[tokio::test(flavor = "multi_thread")]
async fn an_unrelated_request_is_not_blocked_by_a_sweep_in_flight() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    post_batch_to(&state, "slow", json!([note_item("t", "e0")])).await;

    let ready = crate::AppState {
        embedder: crate::EmbedderSlot::ready(Arc::new(SlowEmbedder { dim: 4 })),
        ..state.clone()
    };
    let sweeping = tokio::spawn({
        let ready = ready.clone();
        async move { crate::repair::repair_missing_embeddings(&ready, 8).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = std::time::Instant::now();
    let req = Request::builder()
        .method("GET")
        .uri("/v1/projects")
        .body(Body::empty())
        .unwrap();
    let resp = crate::router(ready.clone()).oneshot(req).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(resp.status(), http::StatusCode::OK);
    assert!(
        elapsed < Duration::from_millis(800),
        "a read that takes the global lock waited {elapsed:?} on a sweep's embed"
    );
    assert_eq!(sweeping.await.expect("join").expect("sweep").repaired, 1);
}

// Capacity is small and shared with every embed-consuming route. A sweep that
// held a permit for its whole run would permanently cut the request path's
// headroom; a sweep that treated a full queue as an error would log noise
// every time the server was merely busy.
#[tokio::test]
async fn a_sweep_stops_quietly_when_the_request_path_holds_every_permit() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    post_batch_to(&state, "busy", json!([note_item("t", "e0")])).await;

    let (slot, _) = recording_slot(4);
    let ready = crate::AppState {
        embedder: slot,
        ..state.clone()
    };
    let held: Vec<_> = (0..crate::EMBED_QUEUE_CAPACITY)
        .map(|_| {
            let Ok(permit) = ready.embed_admission.try_acquire() else {
                panic!("the pool must start with every permit free");
            };
            permit
        })
        .collect();

    let stats = sweep(&ready).await;
    assert!(stats.stopped_early, "{stats:?}");
    assert!(
        stats.stopped_saturated,
        "saturation must be distinguishable from a not-ready embedder: it is \
         the one stop with no future edge, so only the caller re-raising keeps \
         the rest of the backlog reachable; {stats:?}"
    );
    assert_eq!(stats.repaired, 0, "{stats:?}");
    assert_eq!(vectorless_count(&ready).await, 1);

    drop(held);
    assert_eq!(sweep(&ready).await.repaired, 1, "and it retries later");
}

// The failure the distinction above exists to prevent, end to end through the
// worker rather than through a single sweep.
//
// A permit is released silently. If a sweep that met a saturated request path
// simply returned, the rows it had not reached would wait for an unrelated
// future write or a restart, and on a healthy server there is no such write:
// every subsequent one gets its vector at insert time and raises nothing.
// These are the rows a client's sync believes already landed, so nothing
// re-pushes them either.
#[tokio::test]
async fn a_backlog_left_by_a_saturated_request_path_still_completes() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    post_batch_to(&state, "drain", json!([note_item("t", "e0")])).await;

    let (slot, _recorder) = recording_slot(4);
    let ready = crate::AppState {
        embedder: slot,
        ..state.clone()
    };
    assert_eq!(vectorless_count(&ready).await, 1);

    let held: Vec<_> = (0..crate::EMBED_QUEUE_CAPACITY)
        .map(|_| {
            let Ok(permit) = ready.embed_admission.try_acquire() else {
                panic!("the pool must start with every permit free");
            };
            permit
        })
        .collect();

    let worker = tokio::spawn(crate::repair::run_repair_worker(
        ready.clone(),
        8,
        Duration::from_millis(20),
    ));
    ready.repair_signal.raise();

    // Long enough for the worker to wake, meet the saturated path and stop.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        vectorless_count(&ready).await,
        1,
        "nothing can be repaired while every permit is held"
    );

    // Nothing here raises the signal. Only the worker's own re-raise can bring
    // it back, so if the row is repaired after this the re-raise is the reason.
    drop(held);
    for _ in 0..100 {
        if vectorless_count(&ready).await == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        vectorless_count(&ready).await,
        0,
        "once permits free up the worker must come back on its own"
    );
    worker.abort();
}

#[tokio::test]
async fn a_sweep_holds_no_admission_permit_once_it_returns() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    let entries: Vec<Value> = (0..10)
        .map(|i| note_item(&format!("t{i}"), &format!("e{i}")))
        .collect();
    post_batch_to(&state, "permits", json!(entries)).await;

    let (slot, _) = recording_slot(4);
    let ready = crate::AppState {
        embedder: slot,
        ..state.clone()
    };
    assert_eq!(
        crate::repair::repair_missing_embeddings(&ready, 3)
            .await
            .expect("sweep")
            .repaired,
        10
    );

    for _ in 0..crate::EMBED_QUEUE_CAPACITY {
        assert!(
            ready.embed_admission.try_acquire().is_ok(),
            "every permit must be back in the pool after a multi-page sweep"
        );
    }
}

// ── The worker ────────────────────────────────────────────────────────

// A sweep that could not finish must not ask for another, or a durable embed
// outage becomes a loop that reruns the whole backlog as fast as it can fail.
#[tokio::test]
async fn a_sweep_that_makes_no_progress_does_not_ask_for_another() {
    for slot in [
        crate::EmbedderSlot::loading(),
        crate::EmbedderSlot::ready(Arc::new(FailingEmbedder { dim: 4 })),
    ] {
        let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
        post_batch_to(&state, "noloop", json!([note_item("t", "e0")])).await;
        let ready = crate::AppState {
            embedder: slot,
            ..state.clone()
        };
        // Consume the raise the degraded write left behind, so what remains is
        // only what the sweep itself did.
        assert!(signal_is_pending(&ready.repair_signal).await);

        sweep(&ready).await;
        assert!(
            !signal_is_pending(&ready.repair_signal).await,
            "a sweep must never re-signal itself"
        );
    }
}

// The worker is spawned unconditionally, so on a build with no embedder at all
// it has to be inert rather than parked: no sweeps, no logs, and a task that
// simply ends.
#[tokio::test]
async fn the_worker_is_inert_on_a_build_with_no_embedder() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::disabled());
    post_batch_to(&state, "nobackend", json!([note_item("t", "e0")])).await;
    state.repair_signal.raise();

    let ended = tokio::time::timeout(
        Duration::from_millis(200),
        crate::repair::run_repair_worker(state.clone(), 8, Duration::ZERO),
    )
    .await;
    assert!(
        ended.is_ok(),
        "a disabled slot has no transition to wait for, so the worker must return"
    );
    assert_eq!(vectorless_count(&state).await, 1);
}

// An idle server must cost nothing: no signal, no sweep, no embedder call.
#[tokio::test]
async fn the_worker_does_nothing_until_it_is_signalled() {
    let (slot, embedded) = recording_slot(4);
    let state = make_state_with_slot(4, slot);
    post_batch_to(&state, "quiet", json!([note_item("t", "e0")])).await;
    embedded.lock().unwrap().clear();

    let worker = tokio::spawn(crate::repair::run_repair_worker(
        state.clone(),
        8,
        Duration::ZERO,
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        embedded.lock().unwrap().is_empty(),
        "an unsignalled worker must not touch the embedder"
    );
    worker.abort();
}

// And the other half of the same claim: a signal does reach it.
#[tokio::test]
async fn the_worker_sweeps_when_signalled() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    post_batch_to(&state, "woken", json!([note_item("t", "e0")])).await;

    let (slot, _) = recording_slot(4);
    let ready = crate::AppState {
        embedder: slot,
        ..state.clone()
    };
    let worker = tokio::spawn(crate::repair::run_repair_worker(
        ready.clone(),
        8,
        Duration::ZERO,
    ));
    ready.repair_signal.raise();

    for _ in 0..50 {
        if vectorless_count(&ready).await == 0 {
            worker.abort();
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    worker.abort();
    panic!("a signalled worker must repair the backlog");
}

// ── The re-push round trip ────────────────────────────────────────────

// The full loop the field exists to close. A client whose entries all landed
// vectorless sees nothing but skips from then on, so the skip has to carry the
// truth, raise the repair signal, and eventually flip to `true` on its own.
#[tokio::test]
async fn a_vectorless_row_reported_on_re_push_is_repaired_and_then_reports_true() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    post_batch_to(&state, "roundtrip", json!([note_item("t", "e0")])).await;
    assert!(signal_is_pending(&state.repair_signal).await);

    let (_, repush) = post_batch_to(&state, "roundtrip", json!([note_item("t", "e0")])).await;
    assert_eq!(repush["results"][0]["status"], json!("skipped"), "{repush}");
    assert_eq!(repush["results"][0]["embedded"], json!(false), "{repush}");
    assert_eq!(repush["skipped"], json!(1), "{repush}");
    assert!(
        signal_is_pending(&state.repair_signal).await,
        "a dedupe hit on a vectorless row is the last chance to notice it, so it \
         must ask for repair even though it wrote nothing"
    );

    let (slot, _) = recording_slot(4);
    let ready = crate::AppState {
        embedder: slot,
        ..state.clone()
    };
    assert_eq!(sweep(&ready).await.repaired, 1);

    {
        let db = ready.db.lock().await;
        let project = db
            .get_project("roundtrip")
            .expect("project")
            .expect("exists");
        assert_eq!(
            db.search_notes(project.id, &[0.5_f32; 4], 10)
                .expect("knn")
                .len(),
            1,
            "the repaired row must now be reachable by the KNN query"
        );
    }

    let (_, after) = post_batch_to(&ready, "roundtrip", json!([note_item("t", "e0")])).await;
    assert_eq!(after["results"][0]["embedded"], json!(true), "{after}");
    assert!(
        !signal_is_pending(&ready.repair_signal).await,
        "and it stops asking for repair once there is nothing to repair"
    );
}

// ── Rows that arrived with their own vector ───────────────────────────

// A client-pushed vector is a third way a row acquires one, and it lands
// without the embedder being consulted at all. "Is this row vectorless?"
// therefore has to be read off the stored row, never off embedder activity or
// off whether an embed was attempted: judged the latter way, every pushed row
// on a cold server looks like a repair candidate and gets its perfectly good
// client vector overwritten.
#[tokio::test]
async fn a_row_that_arrived_with_its_own_vector_is_never_a_repair_candidate() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    let entries = json!([
        {
            "kind": "note",
            "title": "brought its own",
            "body": "b",
            "external_id": "e0",
            "vector": [0.0, 0.0, 0.0, 1.0],
            "vector_model": inkentry_core::embeddings::pushed_vector_model_tag(),
            "vector_precision": inkentry_core::embeddings::PUSHED_VECTOR_PRECISION,
        },
        note_item("text only", "e1"),
    ]);
    let (status, body) = post_batch_to(&state, "mixed", json!(entries)).await;
    assert_eq!(status, 207, "{body}");
    assert_eq!(body["results"][0]["embedded"], json!(true), "{body}");
    assert_eq!(body["results"][1]["embedded"], json!(false), "{body}");

    let candidates = {
        let db = state.db.lock().await;
        db.notes_missing_embeddings(0, 10).expect("candidates")
    };
    assert_eq!(candidates.len(), 1, "{candidates:?}");
    assert_eq!(
        candidates[0].title, "text only",
        "the pushed-vector row is complete and must be left alone: {candidates:?}"
    );

    let (slot, embedded) = recording_slot(4);
    let ready = crate::AppState {
        embedder: slot,
        ..state.clone()
    };
    assert_eq!(sweep(&ready).await.repaired, 1);

    {
        let seen = embedded.lock().unwrap();
        assert!(
            !seen.iter().any(|t| t.contains("brought its own")),
            "a row carrying a client vector must never be re-embedded by the sweep: {seen:?}"
        );
    }

    let db = ready.db.lock().await;
    let project_id = db
        .get_project("mixed")
        .expect("project")
        .expect("exists")
        .id;
    let hits = db
        .search_notes(project_id, &[0.0_f32, 0.0, 0.0, 1.0], 10)
        .expect("knn");
    assert_eq!(
        hits.first().map(|n| n.title.as_str()),
        Some("brought its own"),
        "the client's vector must still be the one stored: {hits:?}"
    );
}

#[tokio::test]
async fn a_row_archived_mid_sweep_acquires_no_vector() {
    let state = make_state_with_slot(4, crate::EmbedderSlot::loading());
    post_batch_to(&state, "archrace", json!([note_item("t", "e0")])).await;

    let db = state.db.lock().await;
    let project = db.get_project("archrace").expect("p").expect("exists");
    let candidates = db.notes_missing_embeddings(0, 10).expect("candidates");
    let listed = db.list_notes(project.id, None, 10, false).expect("list");
    db.archive_note(project.id, &listed[0].id).expect("archive");

    assert!(
        !db.insert_embedding_if_missing(candidates[0].rowid, &[0.5_f32; 4])
            .expect("write-phase insert"),
        "an archived row is no longer a repair candidate, so it gains no vector"
    );
}
