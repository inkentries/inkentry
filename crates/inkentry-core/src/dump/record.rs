//! The wire shape of a portable dump, as `docs/dump-format.md` defines it.
//!
//! Two spellings here are load-bearing rather than stylistic:
//!
//! * The `record` tag is a closed enum, so an unrecognised record kind fails to
//!   parse. That is deliberate and it is the opposite of the usual JSONL
//!   convention: this format handles compatibility with `format_version`, so a
//!   record kind we do not know means the file is not what it claims to be.
//! * `deny_unknown_fields` is **not** set. Within a version, change is additive
//!   and a reader must tolerate an optional field it does not know.

use serde::Deserialize;

pub const FORMAT: &str = "portable-dump";
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
pub struct Header {
    pub format: String,
    pub format_version: u32,
    #[allow(dead_code)]
    pub generated_at: i64,
    /// Informational. Nothing may branch on it: a reader that changes
    /// behaviour per producer has coupled itself to one, which is the thing
    /// this format exists to avoid.
    #[allow(dead_code)]
    pub generator: String,
}

#[derive(Debug, Deserialize)]
pub struct Footer {
    pub counts: Counts,
    pub digest: String,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Counts {
    pub entity: std::collections::BTreeMap<String, u64>,
    pub relationship: std::collections::BTreeMap<String, u64>,
}

/// A body record: everything between the header and the footer.
#[derive(Debug, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum Record {
    Entity(Box<Entity>),
    Relationship(Relationship),
    /// Present only so a second header or a record after the footer is
    /// reported as the structural error it is, rather than as an unknown kind.
    Header(serde::de::IgnoredAny),
    Footer(serde::de::IgnoredAny),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entity {
    MemoryEntry(Box<MemoryEntry>),
    Project(Project),
    CommandUsage(CommandUsage),
}

impl Entity {
    /// The dump-local wiring token relationships name their endpoints by.
    /// Opaque: never persisted, never parsed, never matched outside its file.
    pub fn dump_ref(&self) -> &str {
        match self {
            Entity::MemoryEntry(e) => &e.dump_ref,
            Entity::Project(e) => &e.dump_ref,
            Entity::CommandUsage(e) => &e.dump_ref,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Entity::MemoryEntry(_) => "memory_entry",
            Entity::Project(_) => "project",
            Entity::CommandUsage(_) => "command_usage",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct MemoryEntry {
    #[serde(rename = "ref")]
    pub dump_ref: String,
    /// Absent means the reader assigns one, seeded from `created_at`.
    pub uuid: Option<String>,
    pub kind: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub linked_files: Vec<String>,
    /// Required precisely so identity can be seeded from it.
    pub created_at: i64,
    pub status: Option<String>,
    pub source_ref: Option<String>,
    pub valid_at: Option<i64>,
    pub invalid_at: Option<i64>,
    /// Content-addressed convergence key, carried verbatim and never
    /// recomputed when present: recomputing it differently would silently fork
    /// every entry.
    pub entity_id: Option<String>,
    pub remote_id: Option<String>,
    #[allow(dead_code)]
    pub namespace: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Project {
    #[serde(rename = "ref")]
    pub dump_ref: String,
    pub root_path: String,
    #[allow(dead_code)]
    pub registered_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct CommandUsage {
    #[serde(rename = "ref")]
    pub dump_ref: String,
    pub command: String,
    pub at: i64,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Relationship {
    #[serde(rename = "type")]
    pub kind: RelationshipKind,
    /// For `supersedes`, the successor.
    pub from: String,
    /// For `supersedes`, the entry being replaced.
    pub to: String,
    pub created_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipKind {
    Supersedes,
    RelatesTo,
    Contradicts,
    DependsOn,
}

impl RelationshipKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RelationshipKind::Supersedes => "supersedes",
            RelationshipKind::RelatesTo => "relates_to",
            RelationshipKind::Contradicts => "contradicts",
            RelationshipKind::DependsOn => "depends_on",
        }
    }
}
