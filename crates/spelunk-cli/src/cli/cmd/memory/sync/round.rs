//! `sync_round`: `spelunk sync`'s two-phase pull/push/pull sequence.

use anyhow::{Context, Result};

use crate::storage::{CloudSyncClient, MemoryStore};

use super::pull::pull_and_apply_since;
use super::push::{PushSummary, push_local};

/// Outcome of one [`sync_round`]: the push summary plus the total newly
/// applied entries across both pull passes.
#[derive(Debug)]
pub(super) struct SyncRoundOutcome {
    pub(super) pushed: PushSummary,
    pub(super) pulled: usize,
}

/// Run one full two-way sync round: pull, then push, then pull again off the
/// same pre-round cursor.
///
/// This is `spelunk sync`'s actual push+pull sequence, extracted into its own
/// function so it can be exercised directly in tests against a real server:
/// the command entry point (`memory_sync`) can't be unit-tested cheaply
/// because of its config/tier-probe plumbing (`get_tier`'s per-process cache
/// makes multiple differently-configured in-process probes unreliable within
/// one test binary).
///
/// Neither a plain push-then-pull nor a plain pull-then-push reorder is
/// sufficient here. The failure mode: the cursor is always `MAX(remote_id)`
/// over local rows (decision #183, no persisted watermark), and this
/// client's own push mints `remote_id`s stamped "now", chronologically the
/// newest thing on the server. If a plain re-derived cursor were used for a
/// second pull, this round's own just-pushed rows would become the new
/// `MAX(remote_id)`, permanently shadowing (via the strict `>` comparison)
/// any teammate entry that landed between this round's first pull and its
/// own push.
///
/// The fix: capture the cursor once, before this round's own pull or push
/// touches anything (`pre_round_cursor`), pull with it, push, then pull AGAIN
/// reusing that SAME `pre_round_cursor`, not a freshly re-derived one. The
/// second pull harmlessly re-fetches this round's own just-pushed rows (their
/// `remote_id` is now `> pre_round_cursor`) alongside anything a teammate
/// pushed in the gap; both are idempotent no-ops or genuine new applies via
/// [`pull_and_apply_since`], so the combined count is never inflated by
/// double-counting.
pub(super) async fn sync_round(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
    accepts_pushed_vectors: bool,
) -> Result<SyncRoundOutcome> {
    let pre_round_cursor = local.max_remote_id()?;

    let pulled_first = pull_and_apply_since(local, client, pre_round_cursor.as_deref()).await?;

    let pushed = push_local(local, client, include_archived, accepts_pushed_vectors).await?;

    // If this second pull errors (network blip, transient 5xx), the error
    // propagates out of `sync_round` rather than being swallowed — `?`
    // surfaces it to `memory_sync`, which reports a failure and a non-zero
    // exit. That is correct (a real error must not be silently dropped), but
    // by this point `pushed` already reflects a push that may have durably
    // landed server-side (and stamped local `remote_id`s accordingly, inside
    // `push_local`, before this call even runs) — the failure is scoped to
    // the confirmation pull, not the push. Attach that context so the
    // surfaced error doesn't read as "nothing happened": a caller shouldn't
    // conclude their content was lost and try to force a re-push (harmless
    // but pointless — already-stamped rows are excluded from `live` and
    // skipped) instead of simply re-running sync, which retries the pull with
    // an unaffected, freshly-derived cursor.
    let pulled_second = pull_and_apply_since(local, client, pre_round_cursor.as_deref())
        .await
        .with_context(|| {
            format!(
                "confirmation pull failed after this round's push already reached \
                 the server ({} attempted: {} created, {} skipped, {} failed) — \
                 the push is not affected by this error; re-running sync will retry \
                 the pull without re-pushing already-landed entries",
                pushed.attempted, pushed.created, pushed.skipped, pushed.failed
            )
        })?;

    Ok(SyncRoundOutcome {
        pushed,
        pulled: pulled_first + pulled_second,
    })
}

#[cfg(test)]
mod tests {
    use super::super::pull::pull_and_apply;
    use super::super::test_support::{fresh_store, spawn_spelunk_server};
    use super::*;

    // ── sync_round: two-phase reconciliation ────────────────────────────────
    // `sync_round` is `memory_sync`'s actual push+pull sequence, extracted so
    // it can be driven directly against a real spawned server. `memory_sync`
    // itself can't cheaply carry these scenarios: `capability::get_tier`
    // caches its probe result in a per-process `OnceCell`, so several
    // differently-configured in-process probes in one test binary would see
    // stale tiers from whichever test's probe ran first.

    /// The primary repro, fixed: a client with local-only, never-pushed
    /// content, running the actual `sync_round` sequence against a project
    /// that already has a teammate's prior entry (pushed strictly before
    /// this round begins),
    /// ends the round with that teammate entry applied - not 0. This is
    /// exactly the case the existing `two_established_clients_...` test
    /// deliberately routes around (see its own comment) because, before this
    /// fix, `memory_sync`'s push-then-pull order shadowed it permanently.
    #[tokio::test]
    async fn sync_round_pulls_teammates_prior_entry_on_a_first_round_with_local_content() {
        let addr = spawn_spelunk_server().await;
        let base_url = format!("http://{addr}");

        // Teammate A establishes the project first, entirely before client
        // C's own sync round begins.
        let (_tmp_a, store_a) = fresh_store();
        store_a
            .add_note(
                "decision",
                "A1",
                "teammate's prior entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let client_a = CloudSyncClient::new(&base_url, "proj-primary", None, None).unwrap();
        assert_eq!(
            push_local(&store_a, &client_a, false, false)
                .await
                .unwrap()
                .created,
            1
        );

        // Client C has its own never-pushed local entry and has never synced.
        let (_tmp_c, store_c) = fresh_store();
        store_c
            .add_note(
                "decision",
                "C1",
                "client C's own new entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let client_c = CloudSyncClient::new(&base_url, "proj-primary", None, None).unwrap();

        let outcome = sync_round(&store_c, &client_c, false, false).await.unwrap();
        assert_eq!(outcome.pushed.created, 1, "C's own entry must land");
        assert_eq!(
            outcome.pulled, 1,
            "C must pull A's prior entry within this same round, not 0"
        );
        let titles: Vec<String> = store_c
            .rows_for_sync(false)
            .unwrap()
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert!(titles.contains(&"A1".to_string()) && titles.contains(&"C1".to_string()));
    }

    /// Idempotence + no double-counting: running `sync_round` twice back to
    /// back with nothing new to push or pull is a no-op both times, and the
    /// round's own just-pushed row (harmlessly re-fetched by the second,
    /// pre-round-cursor pull) is never counted twice or duplicated locally.
    #[tokio::test]
    async fn sync_round_twice_with_nothing_new_is_idempotent_and_never_double_counts() {
        let addr = spawn_spelunk_server().await;
        let base_url = format!("http://{addr}");

        let (_tmp, store) = fresh_store();
        store
            .add_note("decision", "A1", "own entry", &[], &[], None, None)
            .unwrap();
        let client = CloudSyncClient::new(&base_url, "proj-idem", None, None).unwrap();

        let r1 = sync_round(&store, &client, false, false).await.unwrap();
        assert_eq!(r1.pushed.created, 1);
        assert_eq!(
            r1.pulled, 0,
            "the second pull re-fetches this round's own just-pushed row via \
             the pre-round cursor, but it must not be double-counted"
        );
        assert_eq!(store.count().unwrap(), 1, "no duplicate local row");

        let r2 = sync_round(&store, &client, false, false).await.unwrap();
        assert_eq!(
            (r2.pushed.attempted, r2.pushed.already_synced, r2.pulled),
            (0, 1, 0),
            "a second round with nothing new must be a full no-op"
        );
        assert_eq!(store.count().unwrap(), 1);
    }

    /// The race window a plain reorder cannot close: a teammate's push
    /// that lands on the server strictly between this round's own first pull
    /// and its own push must still be picked up within this same round (via
    /// the second pull, reusing the pre-round cursor) rather than being
    /// permanently shadowed by the round's own push becoming the new
    /// `MAX(remote_id)`.
    ///
    /// Real network concurrency can't be forced deterministically in a unit
    /// test, so this composes `sync_round`'s exact same three calls
    /// (`pull_and_apply_since` / `push_local` / `pull_and_apply_since`,
    /// reusing one `pre_round_cursor`) with the teammate's push manually
    /// interleaved at the precise point the race window occupies.
    #[tokio::test]
    async fn sync_round_catches_a_teammate_push_landing_between_its_own_pull_and_push() {
        let addr = spawn_spelunk_server().await;
        let base_url = format!("http://{addr}");

        let (_tmp, store) = fresh_store();
        store
            .add_note("decision", "Client1", "own new entry", &[], &[], None, None)
            .unwrap();
        let client = CloudSyncClient::new(&base_url, "proj-race", None, None).unwrap();

        // Step 1 of sync_round: capture the cursor, then pull. Nothing on the
        // server yet.
        let pre_round_cursor = store.max_remote_id().unwrap();
        let pulled_first = pull_and_apply_since(&store, &client, pre_round_cursor.as_deref())
            .await
            .unwrap();
        assert_eq!(pulled_first, 0);

        // The race window: a teammate pushes here, strictly between this
        // round's own pull and its own push.
        let (_tmp_b, store_b) = fresh_store();
        store_b
            .add_note(
                "decision",
                "B1",
                "teammate's race-window entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let client_b = CloudSyncClient::new(&base_url, "proj-race", None, None).unwrap();
        assert_eq!(
            push_local(&store_b, &client_b, false, false)
                .await
                .unwrap()
                .created,
            1
        );

        // Step 2 of sync_round: this round's own push.
        let pushed = push_local(&store, &client, false, false).await.unwrap();
        assert_eq!(pushed.created, 1);

        // Step 3 of sync_round: the second pull, reusing pre_round_cursor
        // (NOT a freshly re-derived max_remote_id(), which would now include
        // this round's own push and shadow B1 forever).
        let pulled_second = pull_and_apply_since(&store, &client, pre_round_cursor.as_deref())
            .await
            .unwrap();
        assert_eq!(
            pulled_second, 1,
            "the race-window teammate push must be caught by the second pull, \
             not permanently lost"
        );

        let titles: Vec<String> = store
            .rows_for_sync(false)
            .unwrap()
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert!(titles.contains(&"B1".to_string()));
    }

    /// `memory pull` (one-way, no push) is unaffected by the `sync_round`
    /// two-phase reconciliation added for `sync`. It keeps
    /// deriving a single cursor from the store itself via `pull_and_apply`,
    /// unmodified.
    #[tokio::test]
    async fn pull_and_apply_one_way_pull_still_derives_its_own_single_cursor() {
        let addr = spawn_spelunk_server().await;
        let base_url = format!("http://{addr}");

        let (_tmp_a, store_a) = fresh_store();
        store_a
            .add_note("decision", "A1", "first", &[], &[], None, None)
            .unwrap();
        let client_a = CloudSyncClient::new(&base_url, "proj-pull", None, None).unwrap();
        assert_eq!(
            push_local(&store_a, &client_a, false, false)
                .await
                .unwrap()
                .created,
            1
        );
        store_a
            .add_note("decision", "A2", "second", &[], &[], None, None)
            .unwrap();
        assert_eq!(
            push_local(&store_a, &client_a, false, false)
                .await
                .unwrap()
                .created,
            1
        );

        // A pull-only client with nothing local picks up both in one call.
        let (_tmp_c, store_c) = fresh_store();
        let client_c = CloudSyncClient::new(&base_url, "proj-pull", None, None).unwrap();
        let pulled = pull_and_apply(&store_c, &client_c).await.unwrap();
        assert_eq!(pulled, 2);

        // A second, immediate pull is a no-op (cursor re-derived from what
        // was just applied).
        let pulled_again = pull_and_apply(&store_c, &client_c).await.unwrap();
        assert_eq!(pulled_again, 0);
    }
}
