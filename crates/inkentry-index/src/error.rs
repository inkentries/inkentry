use thiserror::Error;

#[derive(Error, Debug)]
pub enum IndexError {
    #[error("unsupported language: {0}")]
    UnsupportedLanguage(String),

    #[error("parse error in {path}:{line}")]
    ParseError { path: String, line: usize },
}
