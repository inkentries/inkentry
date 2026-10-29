use thiserror::Error;

#[derive(Error, Debug)]
pub enum SearchError {
    #[error("index is empty — run `inkentry index <path>` first")]
    EmptyIndex,

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

#[derive(Error, Debug)]
pub enum InkentryError {
    #[error("backend does not support this operation: {0}")]
    BackendUnsupported(String),

    #[error("schema version {found} is newer than max known {max_known}; upgrade inkentry")]
    SchemaMismatch { found: u8, max_known: u8 },
}
