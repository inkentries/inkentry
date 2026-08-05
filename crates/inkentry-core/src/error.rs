use thiserror::Error;

#[derive(Error, Debug)]
pub enum IndexError {
    #[error("unsupported language: {0}")]
    UnsupportedLanguage(String),

    #[error("parse error in {path}:{line}")]
    ParseError { path: String, line: usize },

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

#[derive(Error, Debug)]
pub enum SearchError {
    #[error("index is empty — run `spelunk index <path>` first")]
    EmptyIndex,

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

#[derive(Error, Debug)]
pub enum SpelunkError {
    #[error("backend does not support this operation: {0}")]
    BackendUnsupported(String),

    #[error("schema version {found} is newer than max known {max_known}; upgrade spelunk")]
    SchemaMismatch { found: u8, max_known: u8 },
}
