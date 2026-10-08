//! Persistence helpers for the typed origin added to the LLM WAL in v11.

use crate::events::LlmCallOrigin;
use crate::worker::store::WorkerStoreError;

pub(super) const MIGRATION_V11_SQL: &str = "ALTER TABLE llm_dispatch ADD COLUMN origin TEXT;";

pub(super) fn encode(origin: &LlmCallOrigin) -> Result<String, WorkerStoreError> {
    serde_json::to_string(origin).map_err(|err| WorkerStoreError::Malformed(err.to_string()))
}

pub(super) fn decode(origin: Option<String>) -> Result<LlmCallOrigin, WorkerStoreError> {
    origin
        .map(|origin| serde_json::from_str(&origin))
        .transpose()
        .map_err(|err| WorkerStoreError::Malformed(format!("invalid LLM origin: {err}")))
        .map(Option::unwrap_or_default)
}
