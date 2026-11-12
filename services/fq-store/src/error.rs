use std::path::PathBuf;

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

    /// The selected backend deliberately does not expose this operation.
    #[error("unsupported operation: {0}")]
    Unsupported(String),

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

    /// An authorization denial: the credential parsed and its signature is
    /// valid, but it does not authorize the operation — no live covering
    /// grant, or the token is expired or attenuated away from the op.
    /// (A malformed/wrongly-signed credential is [`Self::Token`] instead.)
    /// Default-deny surfaces here (M2).
    #[error("permission denied: {0}")]
    Denied(String),

    /// No object is bound to the given name in the index.
    #[error("name not found: {0}")]
    NameNotFound(String),

    /// A store database (`index.db` or `grants.db`) records a schema
    /// version above the highest this binary knows: a newer `fq-cas` wrote
    /// it. Every open refuses such a file, read-only ones included, because
    /// an older binary cannot know what a newer schema means even to read
    /// it. The refusal leaves the file as it was; the only step before the
    /// check is the switch into WAL mode on first connect, which every
    /// `fq-cas` has already made on both files. The remedy is an
    /// `fq-cas` at least as new as the one that wrote it. The runtime's
    /// stores refuse the same case (`BinaryTooOld` in `fq-runtime`'s
    /// `db/schema.rs`).
    #[error(
        "incompatible schema: {path} is at version {db_version}, this binary supports up to \
         {binary_version}. The file was written by a newer fq-cas: run an fq-cas at least as \
         new as that one against this store."
    )]
    BinaryTooOld {
        /// The database file that was refused.
        path: PathBuf,
        /// The schema version the file records.
        db_version: i64,
        /// The highest schema version this binary knows.
        binary_version: i64,
    },
}

impl From<sqlx::Error> for StoreError {
    /// Any database error surfaces as [`StoreError::Index`].
    fn from(e: sqlx::Error) -> Self {
        StoreError::Index(e.to_string())
    }
}
