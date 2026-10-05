// Collapse duplicate-`entity_id` groups in `memory.db` (`inkentry memory
// dedupe`). Never invoked automatically — collapsing is destructive.
//
// No live row may reference a loser once it's deleted. `loser_to_survivor`
// (every id being deleted, mapped to its group's survivor) is computed once
// from the pre-transaction snapshot, so it stays valid regardless of
// processing order. Every `superseded_by` rewrite happens before any delete,
// so losers can then be deleted in any order.
//
// One transaction for the whole run: any error rolls back, `memory.db`
// stays unchanged.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

use super::{MemoryStore, Note, NoteId};
use crate::storage::entity_id::note_entity_id;

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct DedupeSummary {
    pub total_notes: usize,
    pub duplicate_groups: usize,
    pub rows_collapsed: usize,
    pub tags_merged: usize,
    pub linked_files_merged: usize,
    pub supersede_edges_repointed: usize,
    pub supersede_self_edges_dropped: usize,
}

#[cfg(test)]
thread_local! {
    // Fires after group n's writes, before COMMIT: proves rollback under a
    // real multi-group transaction, not just the no-op case.
    static FAULT_AFTER_GROUP: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn inject_fault_after_group(n: usize) {
    FAULT_AFTER_GROUP.with(|f| f.set(Some(n)));
}

#[cfg(test)]
fn clear_fault() {
    FAULT_AFTER_GROUP.with(|f| f.set(None));
}

#[cfg(test)]
fn fault_due(i: usize) -> bool {
    FAULT_AFTER_GROUP.with(|f| f.get() == Some(i))
}

#[cfg(not(test))]
fn fault_due(_i: usize) -> bool {
    false
}

#[cfg(test)]
thread_local! {
    // Fires after loser n is deleted, before the next: proves rollback
    // holds mid-group too, not just at a group boundary.
    static FAULT_AFTER_LOSER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn inject_fault_after_loser(n: usize) {
    FAULT_AFTER_LOSER.with(|f| f.set(Some(n)));
}

#[cfg(test)]
fn clear_loser_fault() {
    FAULT_AFTER_LOSER.with(|f| f.set(None));
}

#[cfg(test)]
fn loser_fault_due(i: usize) -> bool {
    FAULT_AFTER_LOSER.with(|f| f.get() == Some(i))
}

#[cfg(not(test))]
fn loser_fault_due(_i: usize) -> bool {
    false
}

impl MemoryStore {
    // `dry_run` computes the same summary via read-only queries and writes
    // nothing.
    pub fn dedupe_entity_ids(&self, dry_run: bool) -> Result<DedupeSummary> {
        let all = self
            .all_notes_for_dedup()
            .context("reading notes for dedupe")?;
        let total_notes = all.len();

        // Index into `all` rather than moving it: `all` is needed again below
        // for the cross-reference pass. `all_notes_for_dedup` orders by
        // created_at ASC, so each group's first element is already the survivor.
        let mut group_indices: Vec<Vec<usize>> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for (i, n) in all.iter().enumerate() {
            let eid = note_entity_id(n);
            match index.get(&eid) {
                Some(&gi) => group_indices[gi].push(i),
                None => {
                    index.insert(eid, group_indices.len());
                    group_indices.push(vec![i]);
                }
            }
        }
        let duplicate_group_indices: Vec<Vec<usize>> =
            group_indices.into_iter().filter(|g| g.len() > 1).collect();

        let mut summary = DedupeSummary {
            total_notes,
            duplicate_groups: duplicate_group_indices.len(),
            ..Default::default()
        };

        if duplicate_group_indices.is_empty() {
            return Ok(summary);
        }

        let duplicate_groups: Vec<Vec<&Note>> = duplicate_group_indices
            .iter()
            .map(|idxs| idxs.iter().map(|&i| &all[i]).collect())
            .collect();

        // note_group_of classifies a rewrite as "external" for the summary
        // only; it plays no role in correctness.
        let mut loser_to_survivor: HashMap<NoteId, NoteId> = HashMap::new();
        let mut note_group_of: HashMap<NoteId, usize> = HashMap::new();
        let mut survivor_ids: HashSet<NoteId> = HashSet::new();
        for (gi, group) in duplicate_groups.iter().enumerate() {
            let survivor_id = group[0].id.clone();
            survivor_ids.insert(survivor_id.clone());
            for n in group {
                note_group_of.insert(n.id.clone(), gi);
            }
            for loser in &group[1..] {
                loser_to_survivor.insert(loser.id.clone(), survivor_id.clone());
            }
        }

        if dry_run {
            for group in &duplicate_groups {
                self.collapse_group_survivor(group, &loser_to_survivor, &mut summary, false)?;
            }
            self.rewrite_cross_references(
                &all,
                &survivor_ids,
                &loser_to_survivor,
                &note_group_of,
                &mut summary,
                false,
            )?;
            return Ok(summary);
        }

        self.execute_batch("BEGIN IMMEDIATE")
            .context("beginning dedupe transaction")?;
        let result: Result<()> = (|| {
            // Phase 1: merge each group's tags/linked_files/status and
            // resolve its survivor's superseded_by, from the pre-transaction
            // snapshot only, so group order doesn't matter.
            for (i, group) in duplicate_groups.iter().enumerate() {
                self.collapse_group_survivor(group, &loser_to_survivor, &mut summary, true)?;
                if fault_due(i) {
                    anyhow::bail!("injected test fault after group {i}");
                }
            }
            // Phase 2: rewrite every row still pointing at a doomed id,
            // before any loser is deleted, so phase 3 can delete in any order.
            self.rewrite_cross_references(
                &all,
                &survivor_ids,
                &loser_to_survivor,
                &note_group_of,
                &mut summary,
                true,
            )?;
            // Phase 3: delete every loser; phase 2 already cleared every
            // live reference to them.
            for group in &duplicate_groups {
                for (li, loser) in group[1..].iter().enumerate() {
                    let loser_id = &loser.id;
                    self.delete_note(loser_id)?;
                    if loser_fault_due(li) {
                        anyhow::bail!(
                            "injected test fault after deleting loser index {li} within group"
                        );
                    }
                }
            }
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.execute_batch("COMMIT")
                    .context("committing dedupe transaction")?;
                Ok(summary)
            }
            Err(e) => {
                let _ = self.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    // Plans (and, when `apply`, executes) one group's tags/linked_files/status
    // merge and its survivor's final `superseded_by`. `group[0]` is the
    // survivor (created_at ASC). Dry-run and real-run share this path so
    // their counts always agree; only the trailing writes are skipped when
    // `!apply`. Touches nothing outside the group.
    fn collapse_group_survivor(
        &self,
        group: &[&Note],
        loser_to_survivor: &HashMap<NoteId, NoteId>,
        summary: &mut DedupeSummary,
        apply: bool,
    ) -> Result<()> {
        let survivor = group[0];
        let losers = &group[1..];
        let survivor_id = &survivor.id;

        // tags / linked_files: union, add-wins
        let mut new_tags: Vec<String> = Vec::new();
        let mut new_files: Vec<String> = Vec::new();
        for loser in losers {
            for t in &loser.tags {
                if !survivor.tags.contains(t) && !new_tags.contains(t) {
                    new_tags.push(t.clone());
                }
            }
            for f in &loser.linked_files {
                if !survivor.linked_files.contains(f) && !new_files.contains(f) {
                    new_files.push(f.clone());
                }
            }
        }
        summary.tags_merged += new_tags.len();
        summary.linked_files_merged += new_files.len();

        // status: archived sticks
        let any_archived = group.iter().any(|n| n.status == "archived");

        // A candidate resolves through loser_to_survivor to its group's
        // survivor (a no-op if not doomed). A target equal to this group's
        // own survivor is self-referential and dropped; anything else is
        // genuine external.
        let resolve = |v: &NoteId| -> Option<NoteId> {
            let target = loser_to_survivor.get(v).unwrap_or(v);
            if target == survivor_id {
                None
            } else {
                Some(target.clone())
            }
        };

        let external_values: Vec<NoteId> = group
            .iter()
            .filter_map(|n| n.superseded_by.as_ref().and_then(resolve))
            .collect();
        let resolved_survivor_target = external_values.first().cloned();
        if let Some(val) = resolved_survivor_target.as_ref() {
            let conflicting = external_values.iter().any(|v| v != val);
            if conflicting {
                tracing::warn!(
                    "memory dedupe: duplicate-entity_id group for survivor {} carries \
                     conflicting superseded_by values; the earliest-created row's value \
                     ({val}) wins",
                    survivor.id
                );
            }
        }
        // Counts only the survivor's own value resolving to nothing;
        // losers' references are handled (uncounted) by rewrite_cross_references.
        let survivor_self_edge_dropped =
            matches!(survivor.superseded_by.as_ref().map(resolve), Some(None));
        if survivor_self_edge_dropped {
            summary.supersede_self_edges_dropped += 1;
        }

        summary.rows_collapsed += losers.len();

        if !apply {
            return Ok(());
        }

        if !new_tags.is_empty() || !new_files.is_empty() {
            self.union_tags_and_files(survivor_id, &new_tags, &new_files)?;
        }
        if any_archived {
            self.archive(survivor_id)?;
        }
        match resolved_survivor_target {
            Some(val) if survivor.superseded_by.as_ref() != Some(&val) => {
                self.set_superseded_by(survivor_id, &val)?;
            }
            None if survivor.superseded_by.is_some() => {
                // No external fallback in the group: clear rather than leave stale.
                self.clear_superseded_by(survivor_id)?;
            }
            _ => {}
        }

        Ok(())
    }

    // Rewrites every non-survivor row whose `superseded_by` still points at a
    // doomed id, resolved through `loser_to_survivor` so a rewrite always
    // lands on a surviving id. Runs once, globally, before any loser is deleted.
    fn rewrite_cross_references(
        &self,
        all_notes: &[Note],
        survivor_ids: &HashSet<NoteId>,
        loser_to_survivor: &HashMap<NoteId, NoteId>,
        note_group_of: &HashMap<NoteId, usize>,
        summary: &mut DedupeSummary,
        apply: bool,
    ) -> Result<()> {
        for note in all_notes {
            let note_id = &note.id;
            if survivor_ids.contains(note_id) {
                continue; // the survivor's own field is resolved separately
            }
            let Some(v) = note.superseded_by.as_ref() else {
                continue;
            };
            let Some(target) = loser_to_survivor.get(v) else {
                continue; // not a doomed id: nothing to do
            };
            // In-group rewrites are inert clean-up; only cross-group
            // rewrites count as a repoint.
            let same_group = matches!(
                (note_group_of.get(note_id), note_group_of.get(v)),
                (Some(a), Some(b)) if a == b
            );
            if !same_group {
                summary.supersede_edges_repointed += 1;
            }
            if apply {
                self.set_superseded_by(note_id, target)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod superseded_by_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
