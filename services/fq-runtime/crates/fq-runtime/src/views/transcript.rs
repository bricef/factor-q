//! The transcript composition over the worker WAL: the reads that
//! answer "what was said during this invocation".
//!
//! Split out of [`super`] as its own sibling because the conversation
//! reads are a distinct concern from the coordination/liveness folds
//! that make up the rest of the view surface — and because they are the
//! only reads that touch payloads rather than headers, which is the
//! property that decides where they can be served from.
//!
//! Two shapes live here, and the difference between them is the whole
//! point:
//!
//! * [`Views::transcript`] — the full timeline, WAL-backed. The read
//!   service (and through it the dashboard) still rides this.
//! * [`Views::invocation_prompt`] — the opening prompt alone. The
//!   operator surface's transcript is composed from the Turn atom
//!   (`turn.list`), and a prompt is not a Turn, so the Invocation view
//!   is where the edge picks it up.

use super::{Views, ViewsError};
use crate::transcript::{TranscriptEntry, TranscriptPrompt};

impl Views {
    /// The payload-bearing transcript for one invocation, reconstructed
    /// from the worker WAL (`llm_dispatch` + `tool_dispatch` — the only
    /// place payloads persist; those rows outlive archival, so this
    /// works for completed invocations too). `None` when the id has no
    /// dispatch rows at all.
    pub async fn transcript(
        &self,
        invocation_id: &str,
    ) -> Result<Option<Vec<TranscriptEntry>>, ViewsError> {
        let llm = self
            .worker
            .list_llm_dispatches_for_invocation(invocation_id)
            .await?;
        let tools = self
            .worker
            .list_tool_dispatches_for_invocation(invocation_id)
            .await?;
        if llm.is_empty() && tools.is_empty() {
            return Ok(None);
        }
        let mut entries = crate::transcript::collect_transcript(&llm, &tools);

        // Close the story: a terminal invocation gets an explicit
        // Outcome entry so the transcript states whether more turns are
        // expected. The live WAL row (if still present) knows the
        // terminal phase; after archive hand-off the archive row does.
        let terminal = match self.worker.get_invocation_state(invocation_id).await? {
            Some(state) => state.terminal_at.map(|at| (at, state.phase)),
            None => self
                .control_plane
                .get_archive(invocation_id)
                .await?
                .map(|a| (a.terminal_at, a.final_phase)),
        };
        if let Some((timestamp_ms, phase)) = terminal {
            entries.push(TranscriptEntry::Outcome {
                timestamp_ms,
                phase,
            });
        }
        Ok(Some(entries))
    }

    /// The invocation's opening prompt — system prompt and first user
    /// message — from the first `llm_dispatch` row's request payload.
    /// `None` when nothing has been dispatched yet, or when the stored
    /// request carried neither message.
    ///
    /// Every later request re-sends the whole history, so the first row
    /// is the only one worth mining: taking the earliest is what keeps
    /// the prompt from being repeated once per round.
    ///
    /// Reads the dispatch list and keeps its head rather than asking
    /// the store for one row. The store has no LIMIT-1 accessor, and
    /// adding one to buy a first-row read is not worth a second query
    /// shape on a call the operator makes once per transcript — the
    /// rows are ordered by `intent_at` already.
    pub async fn invocation_prompt(
        &self,
        invocation_id: &str,
    ) -> Result<Option<TranscriptPrompt>, ViewsError> {
        let first = self
            .worker
            .list_llm_dispatches_for_invocation(invocation_id)
            .await?
            .into_iter()
            .next();
        Ok(first.and_then(|row| {
            crate::transcript::prompt_from_request(row.intent_at, &row.request_payload)
        }))
    }
}
