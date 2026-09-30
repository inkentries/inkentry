// `superseded_by` reference-resolution edge cases across duplicate-`entity_id`
// groups: adoption self-loop/dangling guards, deletion-order safety, and
// cross-group reference resolution.

use super::test_support::{expect_superseded_by, note_count, open_store};
use super::*;

// A loser's superseded_by pointing at the survivor itself must not be
// adopted verbatim: a self-referencing superseded_by is nonsensical.
#[test]
fn adoption_must_not_selfloop_when_a_loser_points_at_the_survivor() {
    let store = open_store();
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    // loser was (per this row) "superseded by" the survivor itself.
    store.set_superseded_by(&loser, &survivor).unwrap();

    store.dedupe_entity_ids(false).unwrap();

    let note = store.get(&survivor).unwrap().unwrap();
    assert_ne!(
        note.superseded_by,
        expect_superseded_by(&survivor),
        "the survivor's superseded_by must never be adopted as its own id: \
         `resolve` drops a candidate that resolves to the group's survivor"
    );
}

// A loser's superseded_by pointing at a fellow loser (not the survivor) must
// still collapse cleanly rather than leave a live FK reference to a row this
// same transaction deletes.
#[test]
fn adoption_must_not_dangle_when_a_loser_points_at_a_fellow_loser() {
    let store = open_store();
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_a, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_b, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
        .unwrap();
    // loser_a points at fellow loser loser_b: in-group, not external.
    store.set_superseded_by(&loser_a, &loser_b).unwrap();

    let result = store.dedupe_entity_ids(false);

    assert!(
        result.is_ok(),
        "BUG: a loser's superseded_by pointing at a fellow loser in the \
         same group (a chained in-group reference) makes the whole \
         dedupe run fail with a FOREIGN KEY constraint error instead of \
         collapsing cleanly: {:?}. The adoption step blindly copies that \
         in-group value onto the survivor before the deletion loop runs, \
         creating a live FK reference to a row this very transaction \
         then tries to delete.",
        result.as_ref().err()
    );
    if result.is_ok() {
        let note = store.get(&survivor).unwrap().unwrap();
        if let Some(target) = note.superseded_by.as_ref() {
            assert!(
                store.get(target).unwrap().is_some(),
                "survivor.superseded_by ({target}) must not point at a \
                 row that no longer exists"
            );
        }
    }
}

// The survivor's own pre-existing superseded_by points at a fellow in-group
// loser: it must be filtered from adoption and fall through to a genuine
// external candidate elsewhere in the group, not be clobbered back to NULL.
#[test]
fn adoption_survivor_own_in_group_pointer_does_not_clobber_fallthrough_adoption() {
    let store = open_store();
    let (external, _) = store
        .add_note("note", "external target", "b", &[], &[], None, None)
        .unwrap();
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_x, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_y, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
        .unwrap();
    // Survivor's own value points at fellow duplicate loser_x: in-group,
    // must not be adopted verbatim.
    store.set_superseded_by(&survivor, &loser_x).unwrap();
    // loser_y's value is genuinely external: the fall-through adoption
    // target once the in-group value is filtered out.
    store.set_superseded_by(&loser_y, &external).unwrap();

    store.dedupe_entity_ids(false).unwrap();

    let note = store.get(&survivor).unwrap().unwrap();
    assert_eq!(
        note.superseded_by,
        expect_superseded_by(&external),
        "the survivor's in-group pointer (at loser_x) is dropped by `resolve`, so \
         loser_y's external value is adopted, and `rewrite_cross_references` \
         (which skips survivors) must not clear it afterwards"
    );
}

// 3+ losers, with the first candidate in iteration order intra-group-dangling
// and a later one genuinely external: fall-through must still work when the
// in-group pointer is on a loser, not the survivor.
#[test]
fn fallthrough_adoption_skips_intragroup_dangling_candidate_and_adopts_later_external_one() {
    let store = open_store();
    let (external, _) = store
        .add_note("note", "external target", "b", &[], &[], None, None)
        .unwrap();
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_a, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_b, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
        .unwrap();
    let (loser_c, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 400)
        .unwrap();
    // loser_a (first candidate in iteration order) points at a fellow
    // loser: intra-group-dangling, must be skipped.
    store.set_superseded_by(&loser_a, &loser_b).unwrap();
    // loser_c (later in iteration order) carries the only valid external
    // candidate.
    store.set_superseded_by(&loser_c, &external).unwrap();

    store.dedupe_entity_ids(false).unwrap();

    let note = store.get(&survivor).unwrap().unwrap();
    assert_eq!(
        note.superseded_by,
        expect_superseded_by(&external),
        "the intra-group-dangling candidate from loser_a must be skipped \
         and the later, genuinely-external candidate from loser_c adopted"
    );
}

// With only intra-group candidates (no external value at all), adoption
// must resolve to None, not error or keep a bad in-group value.
#[test]
fn adoption_resolves_to_none_when_every_candidate_is_intragroup() {
    let store = open_store();
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_a, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_b, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
        .unwrap();
    // loser_a -> loser_b -> survivor: every candidate resolves to a
    // fellow group member, none external.
    store.set_superseded_by(&loser_a, &loser_b).unwrap();
    store.set_superseded_by(&loser_b, &survivor).unwrap();

    let result = store.dedupe_entity_ids(false);
    assert!(
        result.is_ok(),
        "an all-intra-group candidate set must not error: {:?}",
        result.as_ref().err()
    );
    let note = store.get(&survivor).unwrap().unwrap();
    assert_eq!(
        note.superseded_by, None,
        "with zero valid external candidates in the group, the survivor \
         must adopt None, not error and not retain a dangling/self value"
    );
}

// A later-created loser's own superseded_by points at an earlier-created
// fellow loser: deletion runs in created_at ASC order, so this must not
// break when loser_early is deleted while loser_late still references it.
#[test]
fn later_loser_pointing_at_earlier_fellow_loser_must_not_break_deletion_order() {
    let store = open_store();
    let (_survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_early, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_late, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
        .unwrap();
    // loser_late (deleted second, per created_at ASC) points at
    // loser_early (deleted first): the referencing row outlives, in
    // deletion order, the row it references.
    store.set_superseded_by(&loser_late, &loser_early).unwrap();

    let result = store.dedupe_entity_ids(false);

    assert!(
        result.is_ok(),
        "BUG: a later-created loser pointing at an earlier-created \
         fellow loser breaks the deletion loop's naive created_at-ASC \
         order - deleting loser_early while loser_late (not yet \
         deleted) still references it via superseded_by triggers a \
         live FOREIGN KEY constraint error and the whole run fails \
         instead of collapsing cleanly: {:?}",
        result.as_ref().err()
    );
}

// The same deletion-order hazard with roles swapped: two losers point at
// each other (a 2-cycle), so whichever is deleted first is still
// referenced by the other, regardless of created_at order.
#[test]
fn mutually_referencing_fellow_losers_must_not_break_deletion_order() {
    let store = open_store();
    let (_survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_a, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_b, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
        .unwrap();
    store.set_superseded_by(&loser_a, &loser_b).unwrap();
    store.set_superseded_by(&loser_b, &loser_a).unwrap();

    let result = store.dedupe_entity_ids(false);

    assert!(
        result.is_ok(),
        "BUG: two fellow losers pointing at each other (a 2-cycle) \
         breaks the deletion loop regardless of created_at order - \
         whichever is deleted first is still referenced by the other, \
         not-yet-deleted loser, triggering a FOREIGN KEY constraint \
         error: {:?}",
        result.as_ref().err()
    );
}

// A 4-note group with a mix of intra-group and external superseded_by
// values, including a chain (loser -> loser -> external): resolution must
// still pick a deterministic external value and keep counts accurate.
#[test]
fn four_note_group_with_mixed_intragroup_and_external_pointers_resolves_deterministically() {
    let store = open_store();
    let (external_a, _) = store
        .add_note("note", "external a", "b", &[], &[], None, None)
        .unwrap();
    let (external_b, _) = store
        .add_note("note", "external b", "b", &[], &[], None, None)
        .unwrap();
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser1, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser2, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
        .unwrap();
    let (loser3, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 400)
        .unwrap();
    // loser1 chains to fellow loser2: in-group, skipped for adoption,
    // must not break deletion order.
    store.set_superseded_by(&loser1, &loser2).unwrap();
    // loser2 itself carries a genuinely-external value.
    store.set_superseded_by(&loser2, &external_a).unwrap();
    // loser3 carries a different external value (conflict case).
    store.set_superseded_by(&loser3, &external_b).unwrap();

    let summary = store.dedupe_entity_ids(false).unwrap();
    // Resolution order (created_at ASC): loser1's in-group edge is
    // dropped, loser2's external_a wins over loser3's conflicting
    // external_b.
    let note = store.get(&survivor).unwrap().unwrap();
    assert_eq!(
        note.superseded_by,
        expect_superseded_by(&external_a),
        "the earliest-created external candidate (from loser2) must win, \
         with loser1's in-group chain to loser2 correctly excluded"
    );
    assert_eq!(summary.rows_collapsed, 3);

    // Re-run on an independent store to confirm determinism, not just
    // repeatability within one process.
    let store2 = open_store();
    let (external_a2, _) = store2
        .add_note("note", "external a", "b", &[], &[], None, None)
        .unwrap();
    let (external_b2, _) = store2
        .add_note("note", "external b", "b", &[], &[], None, None)
        .unwrap();
    let (survivor2, _) = store2
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser1b, _) = store2
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser2b, _) = store2
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
        .unwrap();
    let (loser3b, _) = store2
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 400)
        .unwrap();
    store2.set_superseded_by(&loser1b, &loser2b).unwrap();
    store2.set_superseded_by(&loser2b, &external_a2).unwrap();
    store2.set_superseded_by(&loser3b, &external_b2).unwrap();
    store2.dedupe_entity_ids(false).unwrap();
    let note2 = store2.get(&survivor2).unwrap().unwrap();
    assert_eq!(
        note2.superseded_by,
        expect_superseded_by(&external_a2),
        "the resolution must be deterministic across independent runs \
         on an equivalent input shape, not HashMap-iteration-order- \
         dependent"
    );
}

// Two groups in the same call, where group B's member has its own
// superseded_by pointing at a member of group A (processed earlier): group
// B's resolution must not write a value pointing at a row group A's
// processing already deleted in this same transaction.
#[test]
fn external_row_that_is_itself_a_duplicate_in_a_different_group_is_resolved_correctly_across_groups()
 {
    let store = open_store();
    let (survivor_a, _) = store
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_a, _) = store
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (survivor_b, _) = store
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 150)
        .unwrap();
    let (external_x, _) = store
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 250)
        .unwrap();
    // external_x is a loser in group B, but its own pre-existing
    // superseded_by points at loser_a, a member of the unrelated group A.
    store.set_superseded_by(&external_x, &loser_a).unwrap();

    let result = store.dedupe_entity_ids(false);

    assert!(
        result.is_ok(),
        "BUG: group B's adoption resolution read external_x's \
         superseded_by from a stale pre-transaction snapshot (still \
         pointing at loser_a) rather than the live DB (where group A's \
         earlier processing already repointed it to survivor_a and \
         deleted loser_a), so it tried to write survivor_b.superseded_by \
         = loser_a onto a row already deleted in this same transaction: \
         {:?}",
        result.as_ref().err()
    );
    if let Ok(summary) = &result {
        assert_eq!(summary.rows_collapsed, 2, "both groups' losers collapsed");
        let sb = store.get(&survivor_b).unwrap().unwrap();
        assert_eq!(
            sb.superseded_by,
            expect_superseded_by(&survivor_a),
            "survivor_b must end up pointing at survivor_a (the row \
             loser_a was merged into), not the deleted loser_a, and not \
             be left dangling"
        );
    }
}

// Dedupe is never reachable except via this method: `MemoryStore::open` only
// creates or verifies the schema, and no path from it reaches
// `dedupe_entity_ids`.

// A chained cross-group reference: group A's survivor points at a loser of
// group B; group B's survivor independently points at a loser of group C.
// Each edge must resolve in exactly one redirect to the concrete target
// group's survivor, never chasing through what that survivor's own field
// happens to point at.
#[test]
fn chained_cross_group_reference_resolves_one_hop_not_to_intermediate_loser() {
    let store = open_store();
    let (survivor_a, _) = store
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (survivor_b, _) = store
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 150)
        .unwrap();
    let (loser_b, _) = store
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 250)
        .unwrap();
    let (survivor_c, _) = store
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 175)
        .unwrap();
    let (loser_c, _) = store
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 275)
        .unwrap();
    // A's survivor points at B's loser; B's survivor independently
    // points at C's loser. Two separate edges, not a transitive chain.
    store.set_superseded_by(&survivor_a, &loser_b).unwrap();
    store.set_superseded_by(&survivor_b, &loser_c).unwrap();

    let summary = store.dedupe_entity_ids(false).unwrap();
    assert_eq!(
        summary.rows_collapsed, 2,
        "one loser each in groups B and C"
    );

    let a = store.get(&survivor_a).unwrap().unwrap();
    assert_eq!(
        a.superseded_by,
        expect_superseded_by(&survivor_b),
        "A's edge to B's loser must resolve to B's survivor id directly, \
         not the raw (now-deleted) loser_b id"
    );
    let b = store.get(&survivor_b).unwrap().unwrap();
    assert_eq!(
        b.superseded_by,
        expect_superseded_by(&survivor_c),
        "B's own edge to C's loser must resolve to C's survivor \
         independently of A's edge into B"
    );
    assert!(
        store.get(&loser_b).unwrap().is_none() && store.get(&loser_c).unwrap().is_none(),
        "both losers must actually be gone"
    );
}

// An ordinary note that is a member of no duplicate group at all, whose
// superseded_by points at a loser belonging to some other group being
// collapsed in this same run, must still be rewritten: its absence from
// group membership must not be mistaken for "same group".
#[test]
fn ordinary_note_outside_every_group_pointing_at_a_loser_is_rewritten_and_counted() {
    let store = open_store();
    // Bookkeeping noise: unrelated groups to stress note_group_of lookups.
    let (_survivor_a, _) = store
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (_loser_a, _) = store
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 110)
        .unwrap();
    let (survivor_b, _) = store
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 120)
        .unwrap();
    let (loser_b, _) = store
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 130)
        .unwrap();
    let (_survivor_c, _) = store
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 140)
        .unwrap();
    let (_loser_c, _) = store
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 150)
        .unwrap();
    // An ordinary, wholly unique note: not a member of any group above.
    let (ordinary, _) = store
        .add_note("note", "ordinary unique note", "body", &[], &[], None, None)
        .unwrap();
    store.set_superseded_by(&ordinary, &loser_b).unwrap();

    let summary = store.dedupe_entity_ids(false).unwrap();
    assert_eq!(summary.rows_collapsed, 3, "one loser in each of 3 groups");
    assert_eq!(
        summary.supersede_edges_repointed, 1,
        "the ordinary note's edge into group B's loser must count as a \
         genuine (non-same-group) repoint"
    );
    let note = store.get(&ordinary).unwrap().unwrap();
    assert_eq!(
        note.superseded_by,
        expect_superseded_by(&survivor_b),
        "BUG CHECK: an ordinary note with no group membership at all must \
         still be rewritten by rewrite_cross_references; note_group_of.get \
         returning None for this row must not be mistaken for group \
         membership"
    );
}

// Three duplicate groups whose survivors form a reference cycle through each
// other's losers (A -> B's loser, B -> C's loser, C -> A's loser) must
// resolve identically regardless of which group's earliest member sorts
// first, proving the resolution map's construction is order-independent.
#[test]
fn three_group_reference_cycle_resolves_identically_under_two_processing_orders() {
    // Variant 1: groups created in A, B, C order.
    let store1 = open_store();
    let (survivor_a1, _) = store1
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_a1, _) = store1
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 110)
        .unwrap();
    let (survivor_b1, _) = store1
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_b1, _) = store1
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 210)
        .unwrap();
    let (survivor_c1, _) = store1
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 300)
        .unwrap();
    let (loser_c1, _) = store1
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 310)
        .unwrap();
    store1.set_superseded_by(&survivor_a1, &loser_b1).unwrap();
    store1.set_superseded_by(&survivor_b1, &loser_c1).unwrap();
    store1.set_superseded_by(&survivor_c1, &loser_a1).unwrap();

    let summary1 = store1.dedupe_entity_ids(false).unwrap();

    // Variant 2: same relational shape (A -> B's loser -> C's loser ->
    // A's loser), but groups are created in C, A, B physical order.
    let store2 = open_store();
    let (survivor_c2, _) = store2
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_c2, _) = store2
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 110)
        .unwrap();
    let (survivor_a2, _) = store2
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_a2, _) = store2
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 210)
        .unwrap();
    let (survivor_b2, _) = store2
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 300)
        .unwrap();
    let (loser_b2, _) = store2
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 310)
        .unwrap();
    store2.set_superseded_by(&survivor_a2, &loser_b2).unwrap();
    store2.set_superseded_by(&survivor_b2, &loser_c2).unwrap();
    store2.set_superseded_by(&survivor_c2, &loser_a2).unwrap();

    let summary2 = store2.dedupe_entity_ids(false).unwrap();

    assert_eq!(
        summary1.rows_collapsed, summary2.rows_collapsed,
        "same relational shape must collapse the same number of rows \
         regardless of which group's earliest member happens to sort \
         first"
    );
    assert_eq!(summary1.rows_collapsed, 3);

    // Same relational outcome under both physical orderings: each
    // survivor ends up pointing at the *next* group's survivor around
    // the cycle, in both variants.
    let a1 = store1.get(&survivor_a1).unwrap().unwrap();
    let b1 = store1.get(&survivor_b1).unwrap().unwrap();
    let c1 = store1.get(&survivor_c1).unwrap().unwrap();
    assert_eq!(a1.superseded_by, expect_superseded_by(&survivor_b1));
    assert_eq!(b1.superseded_by, expect_superseded_by(&survivor_c1));
    assert_eq!(c1.superseded_by, expect_superseded_by(&survivor_a1));

    let a2 = store2.get(&survivor_a2).unwrap().unwrap();
    let b2 = store2.get(&survivor_b2).unwrap().unwrap();
    let c2 = store2.get(&survivor_c2).unwrap().unwrap();
    assert_eq!(
        a2.superseded_by,
        expect_superseded_by(&survivor_b2),
        "identical relational outcome under the C, A, B physical/vector order"
    );
    assert_eq!(b2.superseded_by, expect_superseded_by(&survivor_c2));
    assert_eq!(c2.superseded_by, expect_superseded_by(&survivor_a2));
}

// A multi-group scenario with a same-group chain (loser_a1 -> loser_a2), a
// cross-group survivor pointer (survivor_a -> loser_c1), and an ordinary note
// outside every group (-> loser_b1): every summary count must match
// hand-derived expectations exactly, not just avoid a crash.
#[test]
fn hand_derived_summary_counts_match_multi_group_scenario_exactly() {
    let store = open_store();
    let (survivor_a, _) = store
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_a1, _) = store
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 110)
        .unwrap();
    let (loser_a2, _) = store
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 120)
        .unwrap();
    let (survivor_b, _) = store
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_b1, _) = store
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 210)
        .unwrap();
    let (survivor_c, _) = store
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 300)
        .unwrap();
    let (loser_c1, _) = store
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 310)
        .unwrap();
    let (ordinary, _) = store
        .add_note("note", "ordinary unique note", "body", &[], &[], None, None)
        .unwrap();

    store.set_superseded_by(&loser_a1, &loser_a2).unwrap();
    store.set_superseded_by(&survivor_a, &loser_c1).unwrap();
    store.set_superseded_by(&ordinary, &loser_b1).unwrap();

    let summary = store.dedupe_entity_ids(false).unwrap();

    assert_eq!(summary.total_notes, 8);
    assert_eq!(summary.duplicate_groups, 3);
    assert_eq!(summary.rows_collapsed, 4, "hand count: 2 + 1 + 1");
    assert_eq!(summary.tags_merged, 0);
    assert_eq!(summary.linked_files_merged, 0);
    assert_eq!(
        summary.supersede_edges_repointed, 1,
        "hand count: only `ordinary`'s edge; loser_a1's same-group edge \
         to loser_a2 must not be counted"
    );
    assert_eq!(
        summary.supersede_self_edges_dropped, 0,
        "hand count: survivor_a's own field resolves to a genuine \
         cross-group external target (survivor_c), not a self-edge"
    );

    // Cross-check the actual field values agree with the hand-derived
    // counts, not just the counts in isolation.
    let a = store.get(&survivor_a).unwrap().unwrap();
    assert_eq!(a.superseded_by, expect_superseded_by(&survivor_c));
    let b = store.get(&survivor_b).unwrap().unwrap();
    assert_eq!(b.superseded_by, None);
    let c = store.get(&survivor_c).unwrap().unwrap();
    assert_eq!(c.superseded_by, None);
    let o = store.get(&ordinary).unwrap().unwrap();
    assert_eq!(o.superseded_by, expect_superseded_by(&survivor_b));
    assert_eq!(note_count(&store), 4, "8 - 4 collapsed = 4 remaining rows");
}

// Two rows in a group can share the exact same created_at (a batch import,
// or two calls landing in the same second). The query orders by created_at
// ASC with no secondary key, so SQL alone doesn't guarantee a stable tie
// order; the storage surrogate (`notes.id`) breaks the tie, so the
// first-inserted row is the survivor on every run, not by query-plan accident.
#[test]
fn tied_created_at_breaks_deterministically_on_the_first_inserted_row() {
    for _ in 0..5 {
        let store = open_store();
        let (first, _) = store
            .add_note_with_created_at(
                "decision",
                "tied dup",
                "body",
                &[],
                &[],
                None,
                "active",
                500,
            )
            .unwrap();
        let (second, _) = store
            .add_note_with_created_at(
                "decision",
                "tied dup",
                "body",
                &[],
                &[],
                None,
                "active",
                500,
            )
            .unwrap();
        assert_ne!(
            first, second,
            "precondition: two distinct rows, not one reused row"
        );

        let summary = store.dedupe_entity_ids(false).unwrap();
        assert_eq!(summary.rows_collapsed, 1);
        assert!(
            store.get(&first).unwrap().is_some(),
            "the first-inserted row must be the deterministic survivor \
             when created_at ties, on every run"
        );
        assert!(
            store.get(&second).unwrap().is_none(),
            "the later-inserted row must be the loser when created_at ties"
        );
    }
}

// Dedupe interacting with rows built via `add_note_superseding`, not just
// `add_note`/`add_note_with_created_at`. Two independent
// `add_note_superseding` calls for byte-identical content (each superseding
// a different OLD row) create a duplicate group whose members both carry an
// *inbound* edge from an OLD row's own superseded_by; dedupe must repoint
// those inbound edges to the survivor exactly like any other external
// reference.
#[test]
fn duplicate_group_built_via_add_note_superseding_repoints_old_rows_to_survivor() {
    let store = open_store();

    let (old1, _) = store
        .add_note("decision", "old one", "old body one", &[], &[], None, None)
        .unwrap();
    let (old2, _) = store
        .add_note("decision", "old two", "old body two", &[], &[], None, None)
        .unwrap();

    let (new1, created1) = store
        .add_note_superseding(
            "decision",
            "dup replacement",
            "dup body",
            &[],
            &[],
            None,
            &old1,
        )
        .unwrap();
    assert!(created1, "fresh row");
    let (new2, created2) = store
        .add_note_superseding(
            "decision",
            "dup replacement",
            "dup body",
            &[],
            &[],
            None,
            &old2,
        )
        .unwrap();
    assert!(
        created2,
        "with the entity_id unique index dropped, identical content still \
         creates a second, distinct row (the duplicate-group precondition \
         for this test)"
    );
    assert_ne!(new1, new2);

    // Precondition: each OLD row now points at its own distinct
    // successor - new1 and new2, which are themselves a duplicate group.
    assert_eq!(
        store.get(&old1).unwrap().unwrap().superseded_by,
        expect_superseded_by(&new1)
    );
    assert_eq!(
        store.get(&old2).unwrap().unwrap().superseded_by,
        expect_superseded_by(&new2)
    );

    let summary = store.dedupe_entity_ids(false).unwrap();
    assert_eq!(summary.duplicate_groups, 1, "new1/new2 form one group");
    assert_eq!(summary.rows_collapsed, 1);

    // new1 (earlier created_at) survives; new2 is the loser.
    assert!(store.get(&new1).unwrap().is_some());
    assert!(store.get(&new2).unwrap().is_none());

    // old2's superseded_by, which pointed at the now-deleted new2, must
    // be repointed to the survivor new1: the cross-reference rewrite
    // exercising an edge that originated from the supersede path rather
    // than a plain add_note/set_superseded_by call.
    assert_eq!(
        store.get(&old2).unwrap().unwrap().superseded_by,
        expect_superseded_by(&new1),
        "old2's supersede edge, created by add_note_superseding, must be \
         repointed off the deleted duplicate onto the survivor"
    );
    assert_eq!(
        store.get(&old1).unwrap().unwrap().superseded_by,
        expect_superseded_by(&new1),
        "old1's own edge already pointed at the survivor and must be \
         unaffected"
    );
}

// `dedupe_entity_ids` recomputes entity_id fresh from {kind,title,body} on
// every call rather than reading the stored column, so a hand-edited row
// whose stored `entity_id` disagrees with its own content is still
// discoverable and collapsible by `inkentry memory dedupe`. Proves that end
// to end, with the stray row simultaneously the target of an unrelated
// row's superseded_by.
#[test]
fn row_with_a_stale_stored_entity_id_is_still_collapsed_by_dedupe() {
    let store = open_store();
    let (existing_id, _) = store
        .add_note_with_created_at(
            "decision",
            "dup entry",
            "same content",
            &[],
            &[],
            None,
            "active",
            100,
        )
        .unwrap();

    // Byte-identical content to `existing_id`, but its stored entity_id
    // column names entirely different content: only a hand-edited store (or
    // an import carrying a foreign column verbatim) can be in this shape.
    let stray_uuid = crate::storage::memory::uuid_v7_at(200);
    store
        .conn
        .execute(
            "INSERT INTO notes (uuid, kind, title, body, created_at, entity_id) \
             VALUES (?1, 'decision', 'dup entry', 'same content', 200, ?2)",
            rusqlite::params![
                stray_uuid,
                crate::storage::entity_id::entity_id("note", "unrelated", "content"),
            ],
        )
        .unwrap();
    let stray_id: NoteId = stray_uuid.parse().unwrap();

    // A third, unrelated row whose superseded_by points AT the stray
    // row: the stray is simultaneously a supersede target.
    let (pointer_id, _) = store
        .add_note("note", "points at stray", "b", &[], &[], None, None)
        .unwrap();
    store.set_superseded_by(&pointer_id, &stray_id).unwrap();

    let summary = store.dedupe_entity_ids(false).unwrap();
    assert_eq!(
        summary.duplicate_groups, 1,
        "dedupe must discover the group despite the stray row's stored \
         entity_id column naming different content"
    );
    assert_eq!(summary.rows_collapsed, 1);
    assert!(
        store.get(&existing_id).unwrap().is_some(),
        "existing_id (earlier-created) survives"
    );
    assert!(
        store.get(&stray_id).unwrap().is_none(),
        "the stray row is collapsed away"
    );
    assert_eq!(
        store.get(&pointer_id).unwrap().unwrap().superseded_by,
        expect_superseded_by(&existing_id),
        "pointer_id's edge to the now-deleted stray row must be \
         repointed to the survivor"
    );
}
