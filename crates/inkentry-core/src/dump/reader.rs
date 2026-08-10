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

    let relationships = dedupe(relationships);
    let relationships = resolve(&entities, relationships)?;

    Ok(Dump {
        entities,
        relationships,
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

fn resolve(
    entities: &[Entity],
    relationships: Vec<Relationship>,
) -> Result<Vec<ResolvedRelationship>> {
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
