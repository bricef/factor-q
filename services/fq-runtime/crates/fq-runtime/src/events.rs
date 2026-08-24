//! The event vocabulary — re-exported from `fq-ops`, plus the one
//! thing about it only the runtime can say.
//!
//! Every shape an event has on the wire lives in
//! [`fq_ops::events`]: the envelope, the payload variants, the
//! subject vocabulary, the transient set. They moved there so a
//! reader that only renders events — the thin `fq` client's tail —
//! links the contract crate and none of the runtime (ADR-0031). The
//! glob below keeps `crate::events::…` the import path for the daemon
//! side, which is where deciding that an event happened, stamping its
//! envelope, and publishing it all still live.
//!
//! What is written here rather than there is the conversion that
//! needs a runtime type: an [`LlmErrorKind`] is a projection of
//! [`crate::llm::LlmError`], so the mapping sits beside the error it
//! reads.

pub use fq_ops::events::*;

impl From<&crate::llm::LlmError> for LlmErrorKind {
    fn from(err: &crate::llm::LlmError) -> Self {
        use crate::llm::LlmError;
        match err {
            LlmError::Auth(_) => Self::Auth,
            LlmError::RateLimited => Self::RateLimited,
            LlmError::InvalidResponse(_) => Self::InvalidResponse,
            LlmError::RequestFailed(_) => Self::RequestFailed,
            LlmError::UnpricedModel(_) => Self::UnpricedModel,
        }
    }
}

#[cfg(test)]
mod tests;
