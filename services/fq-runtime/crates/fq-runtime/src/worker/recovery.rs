//! Worker recovery: categorise in-flight invocations on startup.
//!
//! Per data-architecture.md §3.1 and §7.1, every non-terminal
//! invocation found in `invocation_state` on worker startup
//! falls into one of three categories based on the state of
//! its WAL rows in `tool_dispatch` / `llm_dispatch`:
//!
//! - **Safe-resume** — the latest dispatch row is `intent`
//!   without a matching `dispatched` (or there are no
//!   dispatches yet). The action was about to run but didn't.
//!   Re-run it. (No external side effect happened, so this is
//!   correctness-preserving under the tool-idempotency
//!   constraint.)
//! - **Safe-replay** — every dispatch is `completed`. The
//!   actions ran and their results are durably stored. Feed
//!   the most recent result to the next reducer step.
//! - **Ambiguous** — at least one dispatch is in `dispatched`
//!   state without a matching `completed`. The tool/LLM may
//!   have run partially, fully, or not at all. Surface to the
//!   operator via `invocation.ambiguous`; do NOT auto-recover.
//!
//! The categorisation is a pure function over the WAL rows
//! and the persisted state row. Recovery execution
//! (re-running a safe-resume action, feeding a safe-replay
//! result back to the runner) is the runner's job; this
//! module just classifies.

use crate::worker::store::{
    DispatchStatus, InvocationStateRow, LlmDispatchRow, ToolDispatchRow, WorkerStore,
    WorkerStoreError,
};

/// Outcome of categorising one invocation's WAL state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryCategory {
    /// All dispatches are at `intent` only (or there are no
    /// dispatches). Re-run the latest pending action — or, if
    /// no dispatches at all, resume from the persisted state.
    SafeResume,

    /// All dispatches are `completed`. The latest completed
    /// row's result is the `last_result` to feed to the next
    /// reducer step.
    SafeReplay,

    /// At least one dispatch is `dispatched` without
    /// `completed`. Operator-surface; do not auto-recover.
    Ambiguous,
}

/// Pure categorisation. Inputs are the persisted state row and
/// every dispatch row associated with that invocation.
///
/// The state row is currently informational — categorisation
/// is decided entirely by the WAL. Future iterations may use
/// it to detect the state-persisted-without-WAL gap (see
/// step-5/6 design discussion); for now any row with no WAL
/// rows whatsoever falls into `SafeResume` and the runner
/// resumes from the state with `last_result = None` (which
/// the harness handles cleanly when phase is `Initial`).
pub fn categorise(
    _state: &InvocationStateRow,
    tool_dispatches: &[ToolDispatchRow],
    llm_dispatches: &[LlmDispatchRow],
) -> RecoveryCategory {
    let any_ambiguous = tool_dispatches
        .iter()
        .any(|r| r.status == DispatchStatus::Dispatched)
        || llm_dispatches
            .iter()
            .any(|r| r.status == DispatchStatus::Dispatched);
    if any_ambiguous {
        return RecoveryCategory::Ambiguous;
    }

    let any_intent = tool_dispatches
        .iter()
        .any(|r| r.status == DispatchStatus::Intent)
        || llm_dispatches
            .iter()
            .any(|r| r.status == DispatchStatus::Intent);
    if any_intent {
        return RecoveryCategory::SafeResume;
    }

    // All dispatches are completed (or none exist). Either
    // case resolves to a clean continuation: SafeReplay if
    // there's a result to feed, SafeResume if there's nothing
    // dispatched yet (the runner will start fresh from the
    // persisted state).
    let has_completed = tool_dispatches
        .iter()
        .any(|r| r.status == DispatchStatus::Completed)
        || llm_dispatches
            .iter()
            .any(|r| r.status == DispatchStatus::Completed);
    if has_completed {
        RecoveryCategory::SafeReplay
    } else {
        RecoveryCategory::SafeResume
    }
}

/// One classified in-flight invocation. Carries enough
/// context for the daemon to log a summary, publish an
/// `invocation.ambiguous` event for the ambiguous case, and
/// (in a follow-up) drive the resume runner for the safe
/// cases.
#[derive(Debug, Clone)]
pub struct ClassifiedInvocation {
    pub state: InvocationStateRow,
    pub category: RecoveryCategory,
    pub tool_dispatches: Vec<ToolDispatchRow>,
    pub llm_dispatches: Vec<LlmDispatchRow>,
}

impl ClassifiedInvocation {
    /// For ambiguous cases, return `(stuck_entity, stuck_call_id)`
    /// for the operator-surfaced event payload. Returns `None`
    /// for non-ambiguous cases.
    pub fn ambiguous_context(&self) -> Option<(&'static str, String)> {
        if !matches!(self.category, RecoveryCategory::Ambiguous) {
            return None;
        }
        if let Some(row) = self
            .tool_dispatches
            .iter()
            .find(|r| r.status == DispatchStatus::Dispatched)
        {
            return Some(("tool_dispatch", row.tool_call_id.clone()));
        }
        if let Some(row) = self
            .llm_dispatches
            .iter()
            .find(|r| r.status == DispatchStatus::Dispatched)
        {
            return Some(("llm_dispatch", row.request_id.clone()));
        }
        None
    }
}

/// Scan every non-terminal invocation in `store`, classify
/// each, and return the full set. The store query is
/// transactional per row but not across the whole result —
/// new rows added concurrently are excluded; that's fine for
/// a startup-time scan.
pub async fn scan_in_flight(
    store: &WorkerStore,
) -> Result<Vec<ClassifiedInvocation>, WorkerStoreError> {
    let in_flight = store.find_in_flight_invocations().await?;
    let mut out = Vec::with_capacity(in_flight.len());
    for state in in_flight {
        let tool_dispatches = store
            .list_tool_dispatches_for_invocation(&state.invocation_id)
            .await?;
        let llm_dispatches = store
            .list_llm_dispatches_for_invocation(&state.invocation_id)
            .await?;
        let category = categorise(&state, &tool_dispatches, &llm_dispatches);
        out.push(ClassifiedInvocation {
            state,
            category,
            tool_dispatches,
            llm_dispatches,
        });
    }
    Ok(out)
}

/// Aggregate counts per category, for the daemon startup
/// summary line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CategoryCounts {
    pub safe_resume: u32,
    pub safe_replay: u32,
    pub ambiguous: u32,
}

impl CategoryCounts {
    pub fn record(&mut self, category: RecoveryCategory) {
        match category {
            RecoveryCategory::SafeResume => self.safe_resume += 1,
            RecoveryCategory::SafeReplay => self.safe_replay += 1,
            RecoveryCategory::Ambiguous => self.ambiguous += 1,
        }
    }

    pub fn total(self) -> u32 {
        self.safe_resume + self.safe_replay + self.ambiguous
    }
}

#[cfg(test)]
mod tests {
    //! Pure tests over `categorise`. No DB, no I/O.

    use super::*;

    fn state_row() -> InvocationStateRow {
        InvocationStateRow {
            invocation_id: "inv".to_string(),
            agent_id: "a".to_string(),
            schema_version: 1,
            phase: "awaiting_model".to_string(),
            state_blob: vec![],
            iteration: 0,
            started_at: 0,
            updated_at: 0,
            terminal_at: None,
            workspace_ref: None,
        }
    }

    fn tool_row(status: DispatchStatus) -> ToolDispatchRow {
        ToolDispatchRow {
            invocation_id: "inv".to_string(),
            tool_call_id: "t".to_string(),
            tool_name: "echo".to_string(),
            status,
            parameters: "{}".to_string(),
            result: if status == DispatchStatus::Completed {
                Some("ok".to_string())
            } else {
                None
            },
            is_error: if status == DispatchStatus::Completed {
                Some(false)
            } else {
                None
            },
            intent_at: 1,
            dispatched_at: if status != DispatchStatus::Intent {
                Some(2)
            } else {
                None
            },
            completed_at: if status == DispatchStatus::Completed {
                Some(3)
            } else {
                None
            },
        }
    }

    fn llm_row(status: DispatchStatus) -> LlmDispatchRow {
        LlmDispatchRow {
            invocation_id: "inv".to_string(),
            request_id: "r".to_string(),
            model: "haiku".to_string(),
            status,
            request_payload: "{}".to_string(),
            response: if status == DispatchStatus::Completed {
                Some("{}".to_string())
            } else {
                None
            },
            cost_usd: None,
            is_error: if status == DispatchStatus::Completed {
                Some(false)
            } else {
                None
            },
            intent_at: 1,
            dispatched_at: if status != DispatchStatus::Intent {
                Some(2)
            } else {
                None
            },
            completed_at: if status == DispatchStatus::Completed {
                Some(3)
            } else {
                None
            },
        }
    }

    #[test]
    fn no_dispatches_is_safe_resume() {
        // Initial state: persisted but no actions started yet.
        let cat = categorise(&state_row(), &[], &[]);
        assert_eq!(cat, RecoveryCategory::SafeResume);
    }

    #[test]
    fn intent_only_tool_is_safe_resume() {
        let cat = categorise(&state_row(), &[tool_row(DispatchStatus::Intent)], &[]);
        assert_eq!(cat, RecoveryCategory::SafeResume);
    }

    #[test]
    fn intent_only_llm_is_safe_resume() {
        let cat = categorise(&state_row(), &[], &[llm_row(DispatchStatus::Intent)]);
        assert_eq!(cat, RecoveryCategory::SafeResume);
    }

    #[test]
    fn dispatched_tool_is_ambiguous() {
        let cat = categorise(&state_row(), &[tool_row(DispatchStatus::Dispatched)], &[]);
        assert_eq!(cat, RecoveryCategory::Ambiguous);
    }

    #[test]
    fn dispatched_llm_is_ambiguous() {
        let cat = categorise(&state_row(), &[], &[llm_row(DispatchStatus::Dispatched)]);
        assert_eq!(cat, RecoveryCategory::Ambiguous);
    }

    #[test]
    fn ambiguous_takes_precedence_over_intent() {
        let cat = categorise(
            &state_row(),
            &[
                tool_row(DispatchStatus::Intent),
                tool_row(DispatchStatus::Dispatched),
            ],
            &[],
        );
        assert_eq!(cat, RecoveryCategory::Ambiguous);
    }

    #[test]
    fn ambiguous_takes_precedence_over_completed() {
        let cat = categorise(
            &state_row(),
            &[
                tool_row(DispatchStatus::Completed),
                tool_row(DispatchStatus::Dispatched),
            ],
            &[],
        );
        assert_eq!(cat, RecoveryCategory::Ambiguous);
    }

    #[test]
    fn all_completed_tools_is_safe_replay() {
        let cat = categorise(
            &state_row(),
            &[
                tool_row(DispatchStatus::Completed),
                tool_row(DispatchStatus::Completed),
            ],
            &[],
        );
        assert_eq!(cat, RecoveryCategory::SafeReplay);
    }

    #[test]
    fn mixed_completed_and_intent_is_safe_resume() {
        // The new intent supersedes the older completed:
        // there's a pending action to run, and it isn't
        // ambiguous.
        let cat = categorise(
            &state_row(),
            &[
                tool_row(DispatchStatus::Completed),
                tool_row(DispatchStatus::Intent),
            ],
            &[],
        );
        assert_eq!(cat, RecoveryCategory::SafeResume);
    }

    // ---- Integration ----

    #[tokio::test]
    async fn scan_in_flight_classifies_each_invocation() {
        // Pre-populate a worker store with one invocation per
        // category and verify scan_in_flight returns them
        // correctly classified.
        use crate::worker::store::WorkerStore;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let store = WorkerStore::open(&dir.path().join("events.db"))
            .await
            .unwrap();

        // Three non-terminal invocations.
        for id in ["safe-resume", "safe-replay", "ambiguous"] {
            store
                .upsert_invocation_state(&InvocationStateRow {
                    invocation_id: id.to_string(),
                    agent_id: "a".to_string(),
                    schema_version: 1,
                    phase: "awaiting_model".to_string(),
                    state_blob: vec![],
                    iteration: 0,
                    started_at: 1,
                    updated_at: 1,
                    terminal_at: None,
                    workspace_ref: None,
                })
                .await
                .unwrap();
        }
        // safe-resume: intent only.
        store
            .write_tool_intent("safe-resume", "tc1", "shell", "{}", 10)
            .await
            .unwrap();

        // safe-replay: completed.
        store
            .write_tool_intent("safe-replay", "tc2", "shell", "{}", 10)
            .await
            .unwrap();
        store
            .write_tool_dispatched("safe-replay", "tc2", 11)
            .await
            .unwrap();
        store
            .write_tool_completed("safe-replay", "tc2", "ok", false, 12)
            .await
            .unwrap();

        // ambiguous: dispatched without completed.
        store
            .write_tool_intent("ambiguous", "tc3", "shell", "{}", 10)
            .await
            .unwrap();
        store
            .write_tool_dispatched("ambiguous", "tc3", 11)
            .await
            .unwrap();

        // Also a terminal invocation that should NOT appear.
        let mut terminal_row = InvocationStateRow {
            invocation_id: "terminal".to_string(),
            agent_id: "a".to_string(),
            schema_version: 1,
            phase: "completed".to_string(),
            state_blob: vec![],
            iteration: 0,
            started_at: 1,
            updated_at: 1,
            terminal_at: Some(2),
            workspace_ref: None,
        };
        store.upsert_invocation_state(&terminal_row).await.unwrap();
        terminal_row.invocation_id = "terminal-2".to_string();
        store.upsert_invocation_state(&terminal_row).await.unwrap();

        let classified = scan_in_flight(&store).await.unwrap();
        assert_eq!(
            classified.len(),
            3,
            "terminal invocations must not be in the scan"
        );

        let by_id: std::collections::HashMap<&str, RecoveryCategory> = classified
            .iter()
            .map(|c| (c.state.invocation_id.as_str(), c.category.clone()))
            .collect();

        assert_eq!(by_id["safe-resume"], RecoveryCategory::SafeResume);
        assert_eq!(by_id["safe-replay"], RecoveryCategory::SafeReplay);
        assert_eq!(by_id["ambiguous"], RecoveryCategory::Ambiguous);

        // ambiguous_context returns Some for ambiguous and None
        // otherwise.
        for c in &classified {
            match c.category {
                RecoveryCategory::Ambiguous => {
                    let (entity, call_id) =
                        c.ambiguous_context().expect("ambiguous context populated");
                    assert_eq!(entity, "tool_dispatch");
                    assert_eq!(call_id, "tc3");
                }
                _ => assert!(c.ambiguous_context().is_none()),
            }
        }
    }

    #[test]
    fn category_counts_record_and_total() {
        let mut counts = CategoryCounts::default();
        counts.record(RecoveryCategory::SafeResume);
        counts.record(RecoveryCategory::SafeResume);
        counts.record(RecoveryCategory::SafeReplay);
        counts.record(RecoveryCategory::Ambiguous);
        assert_eq!(counts.safe_resume, 2);
        assert_eq!(counts.safe_replay, 1);
        assert_eq!(counts.ambiguous, 1);
        assert_eq!(counts.total(), 4);
    }
}
