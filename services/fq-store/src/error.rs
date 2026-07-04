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

    /// A conflicting concurrent operation the caller may retry — e.g. a block a
    /// writer was reserving was claimed by the collector at the same moment.
    /// Not corruption: retry, or re-`put` the content.
    #[error("conflict: {0}")]
    Conflict(String),

    /// A remote store failed — either a transport error reaching it, or an
    /// error it reported that has no typed variant here. Distinct from
    /// [`Corrupt`](Self::Corrupt): the fault is at or beyond the remote
    /// boundary, not in local stored data.
    #[error("remote store error: {0}")]
    Remote(String),

    /// The grant-event bus (the fan-out feed) failed — the broker is
    /// unreachable or rejected a publish. By design this never affects store
    /// availability: events stay durably queued locally and drain when the
    /// bus returns (M2).
    #[error("event bus error: {0}")]
    Bus(String),

    /// A capability token (or its key material) is invalid — unparseable,
    /// wrongly signed, or malformed. Distinct from an *authorization* denial:
    /// this is "the credential itself is bad", not "the credential lacks
    /// authority" (M2).
    #[error("token error: {0}")]
    Token(String),

    /// No object is bound to the given name in the index.
    #[error("name not found: {0}")]
    NameNotFound(String),
}

impl From<sqlx::Error> for StoreError {
    /// Any database error surfaces as [`StoreError::Index`].
    fn from(e: sqlx::Error) -> Self {
        StoreError::Index(e.to_string())
    }
}
