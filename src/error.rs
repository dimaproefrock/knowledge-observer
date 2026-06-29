//! Crate-wide error type — self-contained, with no dependency on any host app.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Msg(String),
}

impl Error {
    /// Construct a free-form error message.
    pub fn msg(s: impl Into<String>) -> Self {
        Error::Msg(s.into())
    }
}

/// Crate-wide result alias used by the store/engine modules.
pub type Result<T> = std::result::Result<T, Error>;
