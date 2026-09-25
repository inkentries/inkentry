use std::collections::HashSet;

use anyhow::Result;

use crate::storage::{BatchPushItem, CloudSyncClient, MemoryStore, NoteId, SyncEdgePush};

use super::local_embed::{LocalEmbedPolicy, repair_local_embeddings, usable_vector};

// Small enough that a text-only chunk the server must re-embed on a cold
// embedder stays well under the request timeout, and a with-vectors chunk
// stays a sub-megabyte body.
const PUSH_BATCH_CHUNK_SIZE: usize = 50;

#[derive(Debug)]
pub(in crate::cli::cmd) struct PushSummary {
    // Rows sent to `push_batch`, excluding already-synced ones.
    pub attempted: usize,
    // Tallied from `results[].status`: the server's aggregate ints are
    // independent wire fields and have been seen to disagree with them.
    pub created: u32,
    pub skipped: u32,
    // Any status other than `created`/`skipped`, including unrecognized ones.
    pub failed: u32,
    pub already_synced: usize,
    // `Some(reason)` when a chunk failed and the remaining chunks were not sent.
    pub interrupted: Option<String>,
    // Separate from created/skipped/failed, which describe the destination.
    pub embedded_locally: usize,
    // Always 0 under `LocalEmbedPolicy::Skip`.
    pub without_local_vector: usize,
    pub edges_pushed: usize,
}

pub(in crate::cli::cmd) async fn push_local_oneway(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
    accepts_pushed_vectors: bool,
    force: bool,
    local_embed: &LocalEmbedPolicy<'_>,
) -> Result<PushSummary> {
    push_local_reporting(
        local,
        client,
        include_archived,
        accepts_pushed_vectors,
        force,
        local_embed,
        stderr_progress,
    )
    .await
}

fn stderr_progress(done: usize, total: usize) {
    eprintln!("Pushed {done}/{total}…");
}

pub(super) async fn push_local(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
    accepts_pushed_vectors: bool,
    local_embed: &LocalEmbedPolicy<'_>,
) -> Result<PushSummary> {
    push_local_reporting(
        local,
        client,
        include_archived,
        accepts_pushed_vectors,
        false,
        local_embed,
        stderr_progress,
    )
    .await
}

// `on_progress` is injected so tests can observe progress without capturing stderr.
async fn push_local_reporting(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
    accepts_pushed_vectors: bool,
    force: bool,
    local_embed: &LocalEmbedPolicy<'_>,
    mut on_progress: impl FnMut(usize, usize),
) -> Result<PushSummary> {
    let rows = local.rows_for_sync(include_archived)?;
    if rows.is_empty() {
        return Ok(PushSummary {
            attempted: 0,
            created: 0,
            skipped: 0,
            failed: 0,
            already_synced: 0,
            interrupted: None,
            embedded_locally: 0,
            without_local_vector: 0,
            edges_pushed: 0,
        });
    }

    let mut created = 0u32;
    let mut skipped = 0u32;
    let mut failed = 0u32;
    // A `relates_to` edge becomes pushable in the round its second endpoint
    // lands, so only edges touching this set are new.
    let mut just_synced: HashSet<NoteId> = HashSet::new();
    let mut interrupted: Option<String> = None;

    // `--force` re-offers every active row so a server that lost its database
    // is restored.
    let live: Vec<&_> = rows
        .iter()
        .filter(|r| !r.archived && (force || r.remote_id.is_none()))
        .collect();
    let already_synced = if force {
        0
    } else {
        rows.iter()
            .filter(|r| !r.archived && r.remote_id.is_some())
            .count()
    };
    let attempted = live.len();
    // Must run before the batch is built so minted vectors reach `maybe_attach_vector`.
    let repair = repair_local_embeddings(local, &live, local_embed).await?;
    let multi_chunk = attempted.div_ceil(PUSH_BATCH_CHUNK_SIZE) > 1;
    for chunk in live.chunks(PUSH_BATCH_CHUNK_SIZE) {
        let mut items: Vec<BatchPushItem> = Vec::with_capacity(chunk.len());
        for r in chunk {
            // A wrong-length embedding falls back to text-only rather than
            // failing the whole batch with a 4xx.
            let vector = if accepts_pushed_vectors {
                usable_vector(local.get_embedding(&r.id)?)
            } else {
                None
            };
            items.push(
                BatchPushItem {
                    // Sending the row's prior `remote_id` lets a reset server
                    // re-insert it under its original identity.
                    id: if force { r.remote_id.clone() } else { None },
                    kind: r.kind.clone(),
                    title: r.title.clone(),
                    body: if r.body.is_empty() {
                        None
                    } else {
                        Some(r.body.clone())
                    },
                    external_id: r.id.to_string(),
                    source_commit: r.source_ref.clone(),
                    vector: None,
                    vector_model: None,
                    vector_precision: None,
                }
                .maybe_attach_vector(accepts_pushed_vectors, vector),
            );
        }

        // A chunk failure usually means an overloaded server, so stop here;
        // stamped rows leave `live`, so a re-run resumes from this chunk.
        let res = match client.push_batch(items).await {
            Ok(res) => res,
            Err(e) => {
                interrupted = Some(e.to_string());
                break;
            }
        };

        // The aggregate ints are only a fallback: they can disagree with `results[]`.
        if res.results.is_empty() {
            created += res.created;
            skipped += res.skipped;
            failed += res.failed;
        }

        for item in &res.results {
            match item.status.as_str() {
                "created" => created += 1,
                "skipped" => skipped += 1,
                _ => failed += 1,
            }
            // Stamping is permanent, so an id riding along with any other
            // status must not stamp or the row is never retried.
            let durably_persisted = item.status == "created" || item.status == "skipped";
            if durably_persisted
                && let (Some(ext), Some(cloud_id)) =
                    (item.external_id.as_deref(), item.id.as_deref())
                && let Some(row) = chunk.iter().find(|r| r.id.as_str() == ext)
            {
                local.set_remote_id(&row.id, cloud_id)?;
                just_synced.insert(row.id.clone());
            }
            if item.status == "failed" {
                eprintln!(
                    "  [push-fail] {}",
                    item.external_id.as_deref().unwrap_or("<unknown>")
                );
            }
        }

        if multi_chunk {
            on_progress((created + skipped) as usize, attempted);
        }
    }

    // An archived row with no `remote_id` was never pushed, so has nothing to
    // tombstone. Skipped on an interrupted push since the connection is failing.
    if interrupted.is_none() && include_archived {
        for r in rows.iter().filter(|r| r.archived) {
            if let Some(remote_id) = r.remote_id.as_deref() {
                client.delete_remote(remote_id).await?;
            }
        }
    }

    // Best-effort: the entries already landed, so a failure only warns.
    let edges_pushed = if interrupted.is_none() {
        match push_relates_to_edges(local, client, &just_synced).await {
            Ok(n) => n,
            Err(e) => {
                eprintln!("warning: failed to push relates_to edges to the cloud: {e:#}");
                0
            }
        }
    } else {
        0
    };

    Ok(PushSummary {
        attempted,
        created,
        skipped,
        failed,
        already_synced,
        interrupted,
        embedded_locally: repair.embedded,
        without_local_vector: repair.without_vector,
        edges_pushed,
    })
}

// Only `relates_to` is pushed: `supersedes` rides its entry's lifecycle and
// `contradicts` is server-generated. An `unresolved` edge is not counted and is
// retried once its endpoint syncs.
async fn push_relates_to_edges(
    local: &MemoryStore,
    client: &CloudSyncClient,
    just_synced: &HashSet<NoteId>,
) -> Result<usize> {
    if just_synced.is_empty() {
        return Ok(0);
    }
    let edges: Vec<SyncEdgePush> = local
        .relates_to_edges_for_sync()?
        .into_iter()
        .filter(|e| just_synced.contains(&e.from_id) || just_synced.contains(&e.to_id))
        .map(|e| SyncEdgePush {
            from_external_id: e.from_id.to_string(),
            to_external_id: e.to_id.to_string(),
            kind: "relates_to",
        })
        .collect();
    if edges.is_empty() {
        return Ok(0);
    }
    Ok(client.push_edges(edges).await?.applied())
}

#[cfg(test)]
mod chunking_tests;
#[cfg(test)]
mod counting_tests;
#[cfg(test)]
mod edge_tests;
#[cfg(test)]
mod embed_routing_tests;
#[cfg(test)]
mod embed_scope_tests;
#[cfg(test)]
mod embed_tests;
#[cfg(test)]
mod force_tests;
#[cfg(test)]
mod vector_tests;
