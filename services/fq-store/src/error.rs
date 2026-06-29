use thiserror::Error;

use crate::Cid;

/// Convenience alias for store results.
pub type Result<T> = std::result::Result<T, StoreError>;

/// Errors a [`ContentStore`](crate::ContentStore) can return.
#[derive(Debug, Error)]
pub enum StoreError {
    /// No content is stored for the given id.
    #[error("content not found: {0}")]
    NotFound(Cid),

    /// An underlying I/O failure.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// Stored data is missing or malformed — a block referenced by a
    /// manifest is gone, a manifest won't parse, etc. Indicates corruption
    /// or a bug, never normal "absent" conditions.
    #[error("store corruption: {0}")]
    Corrupt(String),

    /// A storage-index (database) failure.
    #[error("index error: {0}")]
    Index(String),

    /// No object is bound to the given name in the index.
    #[error("name not found: {0}")]
    NameNotFound(String),
}
