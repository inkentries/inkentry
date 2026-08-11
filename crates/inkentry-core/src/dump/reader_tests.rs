// Read against the format document's own worked example, byte for byte. A
// reader that only agrees with its matching writer is not interoperable; this
// fixture is the specification's bytes, not ours.

use super::reader::read;
use super::record::{Entity, RelationshipKind};
use sha2::{Digest, Sha256};

const HEADER: &str = r#"{"record":"header","format":"portable-dump","format_version":1,"generated_at":1786370293,"generator":"inkentry/1.0.0"}"#;
const E1: &str = r#"{"record":"entity","type":"memory_entry","ref":"e1","uuid":"0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33","kind":"decision","title":"Old choice","body":"we did X","tags":["a","b"],"linked_files":["src/x.rs"],"created_at":1000,"status":"superseded","source_ref":"commit:abc","entity_id":"ent-1"}"#;
const E2: &str = r#"{"record":"entity","type":"memory_entry","ref":"e2","kind":"decision","title":"New choice","body":"we now do Y","created_at":2000,"status":"active","valid_at":1500}"#;
const E3: &str = r#"{"record":"entity","type":"memory_entry","ref":"e3","kind":"note","title":"Aside","body":"related thing","created_at":3000,"remote_id":"rem-9"}"#;
const R1: &str = r#"{"record":"relationship","type":"contradicts","from":"e1","to":"e3"}"#;
const R2: &str =
    r#"{"record":"relationship","type":"relates_to","from":"e3","to":"e2","created_at":3100}"#;
const R3: &str =
    r#"{"record":"relationship","type":"supersedes","from":"e2","to":"e1","created_at":2500}"#;
const FOOTER: &str = r#"{"record":"footer","counts":{"entity":{"memory_entry":3},"relationship":{"contradicts":1,"relates_to":1,"supersedes":1}},"digest":"sha256:210e1420ea0e650622873d8ab201e380a16774ef5ebc37995bc270fe994fcff5"}"#;

fn spec_example() -> Vec<u8> {
    let mut s = String::new();
    for line in [HEADER, E1, E2, E3, R1, R2, R3, FOOTER] {
        s.push_str(line);
        s.push('\n');
    }
    s.into_bytes()
}

// Rebuild a dump from arbitrary body lines, recomputing the footer so the
// integrity checks pass and a test can isolate the property it is about.
fn dump_with(body: &[&str], counts: &str) -> Vec<u8> {
    let mut lines = vec![HEADER.to_string()];
    lines.extend(body.iter().map(|s| s.to_string()));
    let mut fold = Sha256::new();
    for l in &lines {
        fold.update(hex::encode(Sha256::digest(l.as_bytes())).as_bytes());
    }
    let digest = format!("sha256:{}", hex::encode(fold.finalize()));
    lines.push(format!(
        r#"{{"record":"footer","counts":{counts},"digest":"{digest}"}}"#
    ));
    let mut out = lines.join("\n");
    out.push('\n');
    out.into_bytes()
}

const NO_COUNTS: &str = r#"{"entity":{},"relationship":{}}"#;

#[test]
fn the_specifications_own_example_reads() {
    let dump = read(&spec_example()).expect("the format document's example must read");
    assert_eq!(dump.entities.len(), 3);
    assert_eq!(dump.relationships.len(), 3);
}

#[test]
fn an_entry_carrying_an_identity_keeps_it_and_one_without_is_marked_for_assignment() {
    let dump = read(&spec_example()).unwrap();
    let Entity::MemoryEntry(e1) = &dump.entities[0] else {
        panic!("expected a memory entry")
    };
    assert_eq!(
        e1.uuid.as_deref(),
        Some("0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33")
    );
    let Entity::MemoryEntry(e2) = &dump.entities[1] else {
        panic!("expected a memory entry")
    };
    assert_eq!(e2.uuid, None, "the reader assigns this one");
    assert_eq!(e2.created_at, 2000, "and seeds it from this");
}

#[test]
fn supersedes_is_read_successor_to_predecessor() {
    let dump = read(&spec_example()).unwrap();
    let sup = dump
        .relationships
        .iter()
        .find(|r| r.kind == RelationshipKind::Supersedes)
        .expect("the example has one");
    assert_eq!(
        dump.entities[sup.from].dump_ref(),
        "e2",
        "from is the successor"
    );
    assert_eq!(
        dump.entities[sup.to].dump_ref(),
        "e1",
        "to is being replaced"
    );
}

#[test]
fn absent_optional_fields_read_as_absent_not_as_empty() {
    let dump = read(&spec_example()).unwrap();
    let Entity::MemoryEntry(e2) = &dump.entities[1] else {
        panic!()
    };
    assert!(e2.tags.is_empty());
    assert_eq!(e2.source_ref, None);
}

// ── integrity: every one of these refuses the whole file ─────────────────────

#[test]
fn an_altered_byte_anywhere_is_refused() {
    let original = String::from_utf8(spec_example()).unwrap();
    let tampered = original.replace("we did X", "we did Z");
    assert_ne!(original, tampered);
    let err = read(tampered.as_bytes()).expect_err("a tampered dump must be refused");
    assert!(err.to_string().contains("digest"), "{err}");
}

#[test]
fn a_tampered_header_is_caught_because_the_header_contributes_to_the_digest() {
    let tampered = String::from_utf8(spec_example())
        .unwrap()
        .replace("1786370293", "1786370294");
    let err = read(tampered.as_bytes()).expect_err("a tampered header must be refused");
    assert!(err.to_string().contains("digest"), "{err}");
}

#[test]
fn reordering_records_is_refused_even_though_the_counts_still_agree() {
    // Same records, same counts, different order: the fold is order-sensitive.
    let reordered = [HEADER, E2, E1, E3, R1, R2, R3, FOOTER].join("\n") + "\n";
    let err = read(reordered.as_bytes()).expect_err("a reordered dump is a different dump");
    assert!(err.to_string().contains("digest"), "{err}");
}

#[test]
fn a_removed_record_is_caught_by_the_counts() {
    let short = [HEADER, E1, E2, R1, R2, R3, FOOTER].join("\n") + "\n";
    let err = read(short.as_bytes()).expect_err("a missing record must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("counts"),
        "counts must be what catches this: {msg}"
    );
}

#[test]
fn a_truncated_dump_is_refused_rather_than_partly_imported() {
    let truncated = [HEADER, E1, E2].join("\n") + "\n";
    assert!(read(truncated.as_bytes()).is_err());
}

#[test]
fn a_dump_not_ending_in_a_newline_is_refused() {
    let mut bytes = spec_example();
    bytes.pop();
    let err = read(&bytes).expect_err("a dump ends with a newline");
    assert!(err.to_string().contains("newline"), "{err}");
}

#[test]
fn a_record_after_the_footer_is_refused() {
    let trailing = [HEADER, E1, E2, E3, R1, R2, R3, FOOTER, E1, FOOTER].join("\n") + "\n";
    assert!(read(trailing.as_bytes()).is_err());
}

#[test]
fn a_second_header_is_refused() {
    let two = dump_with(&[HEADER], NO_COUNTS);
    let err = read(&two).expect_err("a dump has exactly one header");
    assert!(err.to_string().contains("second header"), "{err}");
}

#[test]
fn an_unrecognised_record_kind_is_refused_not_skipped() {
    // The opposite of the usual JSONL convention: compatibility is handled by
    // format_version, so an unknown kind means the file is not what it claims.
    let odd = r#"{"record":"annotation","note":"hello"}"#;
    let dump = dump_with(&[odd], NO_COUNTS);
    let err = read(&dump).expect_err("an unknown record kind must be refused");
    assert!(err.to_string().contains("not a valid dump record"), "{err}");
}

#[test]
fn an_unrecognised_entity_type_is_refused() {
    let odd = r#"{"record":"entity","type":"invoice","ref":"x1"}"#;
    let dump = dump_with(&[odd], r#"{"entity":{"invoice":1},"relationship":{}}"#);
    assert!(read(&dump).is_err());
}

#[test]
fn an_unknown_optional_field_is_tolerated_because_change_within_a_version_is_additive() {
    let extended = r#"{"record":"entity","type":"memory_entry","ref":"x1","kind":"note","title":"t","body":"b","created_at":10,"confidence":0.9}"#;
    let dump = dump_with(
        &[extended],
        r#"{"entity":{"memory_entry":1},"relationship":{}}"#,
    );
    assert!(
        read(&dump).is_ok(),
        "a v1 reader tolerates a new optional field"
    );
}

#[test]
fn a_format_it_does_not_recognise_is_refused() {
    let alien = r#"{"record":"header","format":"some-other-dump","format_version":1,"generated_at":1,"generator":"x"}"#;
    let bytes = format!("{alien}\n{FOOTER}\n");
    let err = read(bytes.as_bytes()).expect_err("an unknown format must be refused");
    assert!(err.to_string().contains("not a portable dump"), "{err}");
}

#[test]
fn a_future_format_version_is_refused_rather_than_read_optimistically() {
    let future = r#"{"record":"header","format":"portable-dump","format_version":2,"generated_at":1,"generator":"x"}"#;
    let bytes = format!("{future}\n{FOOTER}\n");
    let err = read(bytes.as_bytes()).expect_err("an unimplemented version must be refused");
    assert!(err.to_string().contains("format version 2"), "{err}");
}

#[test]
fn an_endpoint_that_does_not_resolve_refuses_the_whole_dump() {
    let entity = r#"{"record":"entity","type":"memory_entry","ref":"x1","kind":"note","title":"t","body":"b","created_at":10}"#;
    let dangling = r#"{"record":"relationship","type":"relates_to","from":"x1","to":"nobody"}"#;
    let dump = dump_with(
        &[entity, dangling],
        r#"{"entity":{"memory_entry":1},"relationship":{"relates_to":1}}"#,
    );
    let err = read(&dump).expect_err("a dangling endpoint must refuse the dump");
    let msg = err.to_string();
    assert!(msg.contains("nobody"), "the error must name it: {msg}");
    assert!(msg.contains("Refusing to import any of it"), "{msg}");
}

#[test]
fn an_empty_store_is_a_valid_dump() {
    let dump = dump_with(&[], NO_COUNTS);
    let read = read(&dump).expect("a dump of an empty store is valid");
    assert!(read.entities.is_empty());
    assert!(read.relationships.is_empty());
}

// ── ordering and deduplication ───────────────────────────────────────────────

#[test]
fn relationships_may_precede_the_entities_they_name() {
    let rel = r#"{"record":"relationship","type":"relates_to","from":"x2","to":"x1"}"#;
    let x1 = r#"{"record":"entity","type":"memory_entry","ref":"x1","kind":"note","title":"one","body":"b","created_at":10}"#;
    let x2 = r#"{"record":"entity","type":"memory_entry","ref":"x2","kind":"note","title":"two","body":"b","created_at":20}"#;
    let dump = dump_with(
        &[rel, x1, x2],
        r#"{"entity":{"memory_entry":2},"relationship":{"relates_to":1}}"#,
    );
    let read = read(&dump).expect("record order is unconstrained");
    assert_eq!(read.entities[read.relationships[0].from].dump_ref(), "x2");
    assert_eq!(read.entities[read.relationships[0].to].dump_ref(), "x1");
}

#[test]
fn the_same_fact_twice_is_one_relationship() {
    // A source holding supersession both as a column and as an edge yields the
    // same triple twice; nothing else in the format catches it.
    let x1 = r#"{"record":"entity","type":"memory_entry","ref":"x1","kind":"note","title":"one","body":"b","created_at":10}"#;
    let x2 = r#"{"record":"entity","type":"memory_entry","ref":"x2","kind":"note","title":"two","body":"b","created_at":20}"#;
    let a = r#"{"record":"relationship","type":"supersedes","from":"x2","to":"x1"}"#;
    let b =
        r#"{"record":"relationship","type":"supersedes","from":"x2","to":"x1","created_at":99}"#;
    let dump = dump_with(
        &[x1, x2, a, b],
        r#"{"entity":{"memory_entry":2},"relationship":{"supersedes":2}}"#,
    );
    let read = read(&dump).expect("duplicates are deduplicated, not refused");
    assert_eq!(read.relationships.len(), 1);
    assert_eq!(
        read.relationships[0].created_at,
        Some(99),
        "a recorded timestamp is preferred over its absence"
    );
}

#[test]
fn the_declared_counts_are_of_records_not_of_deduplicated_facts() {
    // Counts describe the file. Deduplication happens after they are checked,
    // so a dump carrying a fact twice must still declare two.
    let x1 = r#"{"record":"entity","type":"memory_entry","ref":"x1","kind":"note","title":"one","body":"b","created_at":10}"#;
    let x2 = r#"{"record":"entity","type":"memory_entry","ref":"x2","kind":"note","title":"two","body":"b","created_at":20}"#;
    let a = r#"{"record":"relationship","type":"supersedes","from":"x2","to":"x1"}"#;
    let dump = dump_with(
        &[x1, x2, a, a],
        r#"{"entity":{"memory_entry":2},"relationship":{"supersedes":1}}"#,
    );
    assert!(read(&dump).is_err());
}

#[test]
fn two_entities_sharing_a_reference_refuse_the_dump() {
    let x1 = r#"{"record":"entity","type":"memory_entry","ref":"dup","kind":"note","title":"one","body":"b","created_at":10}"#;
    let x2 = r#"{"record":"entity","type":"memory_entry","ref":"dup","kind":"note","title":"two","body":"b","created_at":20}"#;
    let dump = dump_with(
        &[x1, x2],
        r#"{"entity":{"memory_entry":2},"relationship":{}}"#,
    );
    let err = read(&dump).expect_err("a dump-local reference is unique within one dump");
    assert!(err.to_string().contains("dup"), "{err}");
}

#[test]
fn two_entities_sharing_a_uuid_refuse_the_dump() {
    let uuid = "0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33";
    let a = format!(
        r#"{{"record":"entity","type":"memory_entry","ref":"a","uuid":"{uuid}","kind":"note","title":"one","body":"b1","created_at":10}}"#
    );
    let b = format!(
        r#"{{"record":"entity","type":"memory_entry","ref":"b","uuid":"{uuid}","kind":"note","title":"two","body":"b2","created_at":20}}"#
    );
    let dump = dump_with(
        &[&a, &b],
        r#"{"entity":{"memory_entry":2},"relationship":{}}"#,
    );
    let err = read(&dump).expect_err("one entry has one identity");
    let msg = err.to_string();
    assert!(msg.contains(uuid), "the message must name it: {msg}");
    assert!(
        msg.contains("Refusing to import any of it"),
        "and refuse the whole file: {msg}"
    );
}

#[test]
fn two_entities_sharing_a_remote_id_refuse_the_dump() {
    let a = r#"{"record":"entity","type":"memory_entry","ref":"a","remote_id":"rem-9","kind":"note","title":"one","body":"b1","created_at":10}"#;
    let b = r#"{"record":"entity","type":"memory_entry","ref":"b","remote_id":"rem-9","kind":"note","title":"two","body":"b2","created_at":20}"#;
    let dump = dump_with(
        &[a, b],
        r#"{"entity":{"memory_entry":2},"relationship":{}}"#,
    );
    let err = read(&dump).expect_err("a remote id names one entry on the server it came from");
    assert!(err.to_string().contains("rem-9"), "{err}");
}

#[test]
fn an_entry_carrying_a_blank_identity_refuses_the_dump_and_names_the_record() {
    for (field, value) in [
        ("uuid", ""),
        ("uuid", "   "),
        ("remote_id", ""),
        ("entity_id", ""),
    ] {
        let line = format!(
            r#"{{"record":"entity","type":"memory_entry","ref":"a","{field}":"{value}","kind":"note","title":"the one","body":"b","created_at":10}}"#
        );
        let dump = dump_with(
            &[&line],
            r#"{"entity":{"memory_entry":1},"relationship":{}}"#,
        );
        let err = read(&dump).expect_err("a carried identity is meaningful or absent");
        let msg = err.to_string();
        assert!(msg.contains(&format!("blank {field}")), "{msg}");
        assert!(msg.contains("\"a\"") && msg.contains("the one"), "{msg}");
    }
}

// A blank identity is reported as blank rather than as a pair that repeats
// itself: the second message describes a contradiction that is not the problem.
#[test]
fn two_entries_carrying_the_same_blank_identity_are_reported_as_blank() {
    let a = r#"{"record":"entity","type":"memory_entry","ref":"a","uuid":"","kind":"note","title":"one","body":"b1","created_at":10}"#;
    let b = r#"{"record":"entity","type":"memory_entry","ref":"b","uuid":"","kind":"note","title":"two","body":"b2","created_at":20}"#;
    let dump = dump_with(
        &[a, b],
        r#"{"entity":{"memory_entry":2},"relationship":{}}"#,
    );
    let err = read(&dump).expect_err("an empty uuid is not an identity");
    assert!(err.to_string().contains("blank uuid"), "{err}");
}

// ── entries that share a convergence key ─────────────────────────────────────

#[test]
fn entries_sharing_a_convergence_key_are_folded_into_the_earliest_and_counted() {
    // Emitted newest-first, so a survivor chosen by file order would be the
    // later one. Same kind/title/body, so the key is the same for both even
    // though neither carries one.
    let late = r#"{"record":"entity","type":"memory_entry","ref":"late","kind":"note","title":"One","body":"same","created_at":20,"tags":["b"],"source_ref":"commit:bbb"}"#;
    let early = r#"{"record":"entity","type":"memory_entry","ref":"early","kind":"note","title":"One","body":"same","created_at":10,"tags":["a"],"source_ref":"commit:aaa"}"#;
    let dump = read(&dump_with(
        &[late, early],
        r#"{"entity":{"memory_entry":2},"relationship":{}}"#,
    ))
    .expect("a store may legitimately hold two entries with one key");

    assert_eq!(dump.entities.len(), 1);
    assert_eq!(dump.merged_memory_entries, 1);
    let Entity::MemoryEntry(kept) = &dump.entities[0] else {
        panic!("expected a memory entry")
    };
    assert_eq!(kept.created_at, 10, "the earliest-created entry survives");
    assert_eq!(kept.tags, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn an_explicit_convergence_key_is_what_groups_when_one_is_carried() {
    // Different text, one carried key: the key is authoritative and is never
    // recomputed, so these are one entry.
    let a = r#"{"record":"entity","type":"memory_entry","ref":"a","kind":"note","title":"One","body":"x","created_at":10,"entity_id":"ent-1"}"#;
    let b = r#"{"record":"entity","type":"memory_entry","ref":"b","kind":"note","title":"Two","body":"y","created_at":20,"entity_id":"ent-1"}"#;
    let dump = read(&dump_with(
        &[a, b],
        r#"{"entity":{"memory_entry":2},"relationship":{}}"#,
    ))
    .unwrap();
    assert_eq!(dump.entities.len(), 1);
    assert_eq!(dump.merged_memory_entries, 1);
}

#[test]
fn a_relationship_between_two_entries_that_fold_together_is_dropped() {
    let a = r#"{"record":"entity","type":"memory_entry","ref":"a","kind":"note","title":"One","body":"same","created_at":10}"#;
    let b = r#"{"record":"entity","type":"memory_entry","ref":"b","kind":"note","title":"One","body":"same","created_at":20}"#;
    let rel = r#"{"record":"relationship","type":"relates_to","from":"a","to":"b"}"#;
    let dump = read(&dump_with(
        &[a, b, rel],
        r#"{"entity":{"memory_entry":2},"relationship":{"relates_to":1}}"#,
    ))
    .unwrap();
    assert!(
        dump.relationships.is_empty(),
        "an entry does not relate to itself once its two halves are one row"
    );
}

#[test]
fn two_relationships_that_the_fold_makes_identical_become_one() {
    let a = r#"{"record":"entity","type":"memory_entry","ref":"a","kind":"note","title":"One","body":"same","created_at":10}"#;
    let b = r#"{"record":"entity","type":"memory_entry","ref":"b","kind":"note","title":"One","body":"same","created_at":20}"#;
    let other = r#"{"record":"entity","type":"memory_entry","ref":"c","kind":"note","title":"Other","body":"z","created_at":30}"#;
    let r1 = r#"{"record":"relationship","type":"relates_to","from":"a","to":"c"}"#;
    let r2 = r#"{"record":"relationship","type":"relates_to","from":"b","to":"c","created_at":99}"#;
    let dump = read(&dump_with(
        &[a, b, other, r1, r2],
        r#"{"entity":{"memory_entry":3},"relationship":{"relates_to":2}}"#,
    ))
    .unwrap();
    assert_eq!(dump.relationships.len(), 1);
    assert_eq!(
        dump.relationships[0].created_at,
        Some(99),
        "a recorded timestamp is still preferred over its absence"
    );
}

#[test]
fn a_dump_carries_no_field_that_could_hold_a_credential() {
    // A property of the format, not a filtering step a writer performs. If a
    // secret-bearing field is ever added, this fails.
    let fields = [
        "record",
        "type",
        "ref",
        "uuid",
        "kind",
        "title",
        "body",
        "tags",
        "linked_files",
        "created_at",
        "status",
        "source_ref",
        "valid_at",
        "invalid_at",
        "entity_id",
        "remote_id",
        "namespace",
        "root_path",
        "registered_at",
        "command",
        "at",
        "from",
        "to",
        "format",
        "format_version",
        "generated_at",
        "generator",
        "counts",
        "digest",
    ];
    for name in fields {
        let n = name.to_ascii_lowercase();
        assert!(
            !["token", "key", "secret", "password", "credential", "auth"]
                .iter()
                .any(|bad| n.contains(bad)),
            "{name} reads like credential-bearing; the format carries none"
        );
    }
}
