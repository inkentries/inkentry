//! Reading a portable dump, and refusing one that is not whole.
//!
//! Every check here refuses the entire file. A dump is read in one pass before
//! anything is written, so there is no such thing as a partial import: a
//! truncated or altered dump is precisely the case where importing most of the
//! data and saying nothing is the worst available outcome.

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};

use super::record::{
    Counts, Entity, FORMAT, FORMAT_VERSION, Footer, Header, Record, Relationship, RelationshipKind,
};

/// A dump that has passed every integrity check: counts, digest, and endpoint
/// resolution. Relationships are deduplicated and their endpoints are indexes
/// into `entities`, so nothing downstream re-resolves a `ref`.
#[derive(Debug)]
pub struct Dump {
    pub entities: Vec<Entity>,
    pub relationships: Vec<ResolvedRelationship>,
    /// Memory entries that shared a convergence key with another entry in the
    /// same dump and were folded into it. Carried so the import can report a
    /// count that describes what landed rather than what it read.
    pub merged_memory_entries: usize,
}

#[derive(Debug)]
pub struct ResolvedRelationship {
    pub kind: RelationshipKind,
    pub from: usize,
    pub to: usize,
    pub created_at: Option<i64>,
}

pub fn read(bytes: &[u8]) -> Result<Dump> {
    let lines = split_lines(bytes)?;
    if lines.len() < 2 {
        bail!("not a portable dump: a dump has at least a header and a footer");
    }

    let header: Header = serde_json::from_slice(lines[0])
        .map_err(|e| anyhow::anyhow!("the first line is not a dump header: {e}"))?;
    if header.format != FORMAT {
        bail!(
            "not a portable dump: format is {:?}, expected {FORMAT:?}",
            header.format
        );
    }
    if header.format_version != FORMAT_VERSION {
        bail!(
            "dump format version {} is not supported by this build (this reader implements \
             version {FORMAT_VERSION})",
            header.format_version
        );
    }

    let last = lines.len() - 1;
    let footer: Footer = serde_json::from_slice(lines[last])
        .map_err(|e| anyhow::anyhow!("the last line is not a dump footer: {e}"))?;

    let mut entities: Vec<Entity> = Vec::new();
    let mut relationships: Vec<Relationship> = Vec::new();
    for (i, line) in lines[1..last].iter().enumerate() {
        // Line 1 in the file is index 0 here, and the header already took the
        // first line, so the human-facing number is i + 2.
        let n = i + 2;
        let record: Record = serde_json::from_slice(line)
            .map_err(|e| anyhow::anyhow!("line {n} is not a valid dump record: {e}"))?;
        match record {
            Record::Entity(e) => entities.push(*e),
            Record::Relationship(r) => relationships.push(r),
            Record::Header(_) => bail!("line {n} is a second header; a dump has exactly one"),
            Record::Footer(_) => {
                bail!("line {n} is a footer with records after it; the dump is not whole")
            }
        }
    }

    verify_counts(&footer.counts, &entities, &relationships)?;
    verify_digest(&footer.digest, &lines[..last])?;

    let by_ref = index_by_ref(&entities)?;
    // Before the repeat check, so two blank identities are reported as blank
    // rather than as a pair that contradicts itself.
    refuse_blank_identities(&entities)?;
    refuse_repeated_identities(&entities)?;

    let relationships = dedupe(relationships);
    let relationships = resolve(&by_ref, relationships)?;

    let collapsed = collapse_by_convergence_key(entities);
    let relationships = redirect_to_survivors(relationships, &collapsed.remap);

    Ok(Dump {
        entities: collapsed.entities,
        relationships,
        merged_memory_entries: collapsed.merged,
    })
}

/// Split on LF, requiring the trailing one. A dump ends with a newline, and a
/// file that does not is truncated mid-record.
fn split_lines(bytes: &[u8]) -> Result<Vec<&[u8]>> {
    if bytes.is_empty() {
        bail!("the dump is empty");
    }
    let Some(body) = bytes.strip_suffix(b"\n") else {
        bail!("the dump does not end with a newline, so its last record is incomplete");
    };
    Ok(body.split(|b| *b == b'\n').collect())
}

fn verify_counts(
    declared: &Counts,
    entities: &[Entity],
    relationships: &[Relationship],
) -> Result<()> {
    let mut found = Counts::default();
    for e in entities {
        *found.entity.entry(e.type_name().to_string()).or_insert(0) += 1;
    }
    for r in relationships {
        *found
            .relationship
            .entry(r.kind.as_str().to_string())
            .or_insert(0) += 1;
    }
    if &found != declared {
        bail!(
            "the dump's declared counts do not match its records, so it is not whole \
             (declared {}; found {}). Refusing to import any of it.",
            render_counts(declared),
            render_counts(&found)
        );
    }
    Ok(())
}

fn render_counts(c: &Counts) -> String {
    let one = |m: &BTreeMap<String, u64>| {
        if m.is_empty() {
            "none".to_string()
        } else {
            m.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    format!(
        "entities: {}; relationships: {}",
        one(&c.entity),
        one(&c.relationship)
    )
}

/// The fold is over the per-record digests as **hex text**, in file order, and
/// it covers the header but not the footer. Folding the raw 32-byte digests
/// instead yields a different answer, so two conforming-looking
/// implementations would reject each other's files.
fn verify_digest(declared: &str, lines: &[&[u8]]) -> Result<()> {
    let mut fold = Sha256::new();
    for line in lines {
        fold.update(hex::encode(Sha256::digest(line)).as_bytes());
    }
    let found = format!("sha256:{}", hex::encode(fold.finalize()));
    if found != declared {
        bail!(
            "the dump's digest does not match its contents, so it has been altered or \
             truncated (declared {declared}, computed {found}). Refusing to import any of it."
        );
    }
    Ok(())
}

/// Deduplicate on `(type, from, to)`, preferring a recorded timestamp over its
/// absence. A source that holds supersession both as a column and as an edge
/// yields the same fact twice, and nothing else in the format catches it.
fn dedupe(relationships: Vec<Relationship>) -> Vec<Relationship> {
    let mut seen: HashMap<(RelationshipKind, String, String), usize> = HashMap::new();
    let mut out: Vec<Relationship> = Vec::new();
    for r in relationships {
        let key = (r.kind, r.from.clone(), r.to.clone());
        match seen.get(&key) {
            Some(&at) => {
                if out[at].created_at.is_none() {
                    out[at].created_at = r.created_at;
                }
            }
            None => {
                seen.insert(key, out.len());
                out.push(r);
            }
        }
    }
    out
}

fn index_by_ref(entities: &[Entity]) -> Result<HashMap<&str, usize>> {
    let mut by_ref: HashMap<&str, usize> = HashMap::new();
    let mut duplicates: HashSet<&str> = HashSet::new();
    for (i, e) in entities.iter().enumerate() {
        if by_ref.insert(e.dump_ref(), i).is_some() {
            duplicates.insert(e.dump_ref());
        }
    }
    if let Some(dup) = duplicates.iter().next() {
        bail!(
            "two entities share the reference {dup:?}; a dump-local reference is unique \
             within one dump, so this file's relationships cannot be resolved. \
             Refusing to import any of it."
        );
    }
    Ok(by_ref)
}

/// Refuse an entry whose carried identity is present but blank.
///
/// Each of these names the entry in the store it came from, and the format says
/// a writer carries them rather than minting them — so each is either
/// meaningful or absent, and `""` is neither. Nothing downstream can recover:
/// a blank `uuid` reaches the store as a key it cannot look up and surfaces as
/// an inserted note that vanished, and a blank `entity_id` silently collapses
/// every entry carrying one into a single row. The type cannot carry this
/// check, since `NoteId::from_str` accepts a whitespace-only token.
fn refuse_blank_identities(entities: &[Entity]) -> Result<()> {
    for e in entities {
        let Entity::MemoryEntry(m) = e else { continue };
        for (field, value) in [
            ("uuid", m.uuid.as_deref()),
            ("remote_id", m.remote_id.as_deref()),
            ("entity_id", m.entity_id.as_deref()),
        ] {
            if value.is_some_and(|v| v.trim().is_empty()) {
                bail!(
                    "the entry {:?} ({:?}) carries a blank {field}; a carried identity names \
                     an entry in the store it came from, so it is either meaningful or absent. \
                     Refusing to import any of it.",
                    m.dump_ref,
                    m.title
                );
            }
        }
    }
    Ok(())
}

/// Refuse a dump in which two entries claim the same `uuid` or the same
/// `remote_id`.
///
/// Both are stable, cross-store identities the format says a writer carries and
/// never mints, so two records under one of them contradict each other — the
/// same class of error as a repeated `ref`, and it belongs to the same reading
/// pass. Without this the contradiction reaches the UNIQUE index mid-write and
/// surfaces as SQLite's own words, which describe neither the dump nor what to
/// do about it.
fn refuse_repeated_identities(entities: &[Entity]) -> Result<()> {
    let mut uuids: HashSet<&str> = HashSet::new();
    let mut remote_ids: HashSet<&str> = HashSet::new();
    for e in entities {
        let Entity::MemoryEntry(m) = e else { continue };
        if let Some(uuid) = m.uuid.as_deref()
            && !uuids.insert(uuid)
        {
            bail!(
                "two entities share the identity {uuid:?}; one entry has one identity, \
                 so this dump describes a state no store can hold. \
                 Refusing to import any of it."
            );
        }
        if let Some(remote) = m.remote_id.as_deref()
            && !remote_ids.insert(remote)
        {
            bail!(
                "two entities share the remote id {remote:?}; it names one entry on the \
                 server they came from, so this dump describes a state no store can hold. \
                 Refusing to import any of it."
            );
        }
    }
    Ok(())
}

/// The result of folding entries that share a convergence key into one.
struct Collapsed {
    entities: Vec<Entity>,
    /// Original entity index → its index in `entities`. A folded entry maps to
    /// the survivor it was folded into.
    remap: Vec<usize>,
    merged: usize,
}

/// Fold memory entries sharing an `entity_id` into one.
///
/// The store keys entries on that convergence key, `NOT NULL` and UNIQUE, so
/// two entries carrying one key cannot both exist there. The collapse is
/// therefore forced rather than chosen — and it is reachable from real data:
/// the key is computed over kind/title/body, so two harvested entries differing
/// only in `source_ref` land on it. Refusing such a dump would make a
/// legitimate store impossible to move, on a move that happens once.
///
/// The survivor is the earliest-created member, ties broken by `ref`. Taking
/// the first in file order instead would make the outcome depend on the
/// writer's emission order, which the format explicitly leaves unconstrained.
/// Tags and linked files are unioned add-wins from every member, matching what
/// the store does when a fresh entry collides with one already in it.
fn collapse_by_convergence_key(mut entities: Vec<Entity>) -> Collapsed {
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, e) in entities.iter().enumerate() {
        if let Entity::MemoryEntry(m) = e {
            let key = m.entity_id.clone().unwrap_or_else(|| {
                crate::storage::entity_id::entity_id(&m.kind, &m.title, &m.body)
            });
            groups.entry(key).or_default().push(i);
        }
    }

    let mut folded_into: HashMap<usize, usize> = HashMap::new();
    let mut inherits: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for members in groups.into_values() {
        if members.len() == 1 {
            continue;
        }
        let mut ordered = members;
        ordered.sort_by_key(|&i| {
            let Entity::MemoryEntry(m) = &entities[i] else {
                unreachable!("only memory entries are grouped")
            };
            (m.created_at, m.dump_ref.clone())
        });
        let survivor = ordered[0];
        for &loser in &ordered[1..] {
            folded_into.insert(loser, survivor);
            inherits.entry(survivor).or_default().push(loser);
        }
    }

    for (survivor, losers) in &inherits {
        let mut tags: Vec<String> = vec![];
        let mut files: Vec<String> = vec![];
        for &loser in losers {
            let Entity::MemoryEntry(m) = &entities[loser] else {
                unreachable!("only memory entries are grouped")
            };
            tags.extend(m.tags.iter().cloned());
            files.extend(m.linked_files.iter().cloned());
        }
        let Entity::MemoryEntry(keeper) = &mut entities[*survivor] else {
            unreachable!("only memory entries are grouped")
        };
        for t in tags {
            if !keeper.tags.contains(&t) {
                keeper.tags.push(t);
            }
        }
        for f in files {
            if !keeper.linked_files.contains(&f) {
                keeper.linked_files.push(f);
            }
        }
    }

    let merged = folded_into.len();
    let mut remap = vec![usize::MAX; entities.len()];
    let mut kept: Vec<Entity> = Vec::with_capacity(entities.len() - merged);
    let mut kept_index_of: HashMap<usize, usize> = HashMap::new();

    for (i, entity) in entities.into_iter().enumerate() {
        if folded_into.contains_key(&i) {
            continue;
        }
        kept_index_of.insert(i, kept.len());
        remap[i] = kept.len();
        kept.push(entity);
    }
    for (&loser, &survivor) in &folded_into {
        remap[loser] = kept_index_of[&survivor];
    }

    Collapsed {
        entities: kept,
        remap,
        merged,
    }
}

/// Point every endpoint at the entity that survived the collapse, then drop
/// what the redirect made meaningless: a relationship whose two endpoints
/// folded into one entry now names that entry twice, and a redirect can make
/// two relationships identical.
fn redirect_to_survivors(
    relationships: Vec<ResolvedRelationship>,
    remap: &[usize],
) -> Vec<ResolvedRelationship> {
    let mut seen: HashMap<(RelationshipKind, usize, usize), usize> = HashMap::new();
    let mut out: Vec<ResolvedRelationship> = Vec::new();
    for r in relationships {
        let (from, to) = (remap[r.from], remap[r.to]);
        if from == to {
            continue;
        }
        match seen.get(&(r.kind, from, to)) {
            Some(&at) => {
                if out[at].created_at.is_none() {
                    out[at].created_at = r.created_at;
                }
            }
            None => {
                seen.insert((r.kind, from, to), out.len());
                out.push(ResolvedRelationship {
                    kind: r.kind,
                    from,
                    to,
                    created_at: r.created_at,
                });
            }
        }
    }
    out
}

fn resolve(
    by_ref: &HashMap<&str, usize>,
    relationships: Vec<Relationship>,
) -> Result<Vec<ResolvedRelationship>> {
    relationships
        .into_iter()
        .map(|r| {
            // A dangling endpoint means the file is damaged or its writer is
            // broken. Both are conditions where importing the rest silently is
            // worse than importing none of it.
            let from = *by_ref.get(r.from.as_str()).ok_or_else(|| {
                anyhow::anyhow!(
                    "a {} relationship names {:?}, which is not an entity in this dump. \
                     Refusing to import any of it.",
                    r.kind.as_str(),
                    r.from
                )
            })?;
            let to = *by_ref.get(r.to.as_str()).ok_or_else(|| {
                anyhow::anyhow!(
                    "a {} relationship names {:?}, which is not an entity in this dump. \
                     Refusing to import any of it.",
                    r.kind.as_str(),
                    r.to
                )
            })?;
            Ok(ResolvedRelationship {
                kind: r.kind,
                from,
                to,
                created_at: r.created_at,
            })
        })
        .collect()
}
