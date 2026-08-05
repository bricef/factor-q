//! The transcript composition over the worker WAL: the reads that
//! answer "what was said during this invocation".
//!
//! Split out of [`super`] as its own sibling because the conversation
//! reads are a distinct concern from the coordination/liveness folds
//! that make up the rest of the view surface — and because they are the
//! only reads that touch payloads rather than headers, which is the
//! property that decides where they can be served from.
//!
//! One shape lives here: [`Views::transcript`], the full timeline,
//! WAL-backed. The read service (and through it the dashboard) still
//! rides this. The operator surface does not — its transcript is
//! composed from the Turn atom (`turn.list`), opening prompt included.

use super::{Views, ViewsError};
use crate::transcript::TranscriptEntry;

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
}
