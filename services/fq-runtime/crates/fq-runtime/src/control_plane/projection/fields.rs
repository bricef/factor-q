//! The event → column mapping: which denormalised fields each payload
//! variant contributes to a projected row.
//!
//! Split out of `store.rs` — the match is one exhaustive arm per event
//! type and grows with the schema, while the store around it is
//! queries. Keeping them in one file made every new event type pay
//! that file's size budget.
//!
//! The tail arm is an explicit variant *list*, never `_`: a new event
//! type must be looked at and given columns, or deliberately given
//! none.

use serde::Serialize;

use crate::events::{Event, EventPayload};

/// Denormalised fields extracted from an event for indexing.
#[derive(Default)]
pub(super) struct Fields {
    pub(super) model: Option<String>,
    pub(super) input_tokens: Option<i64>,
    pub(super) output_tokens: Option<i64>,
    pub(super) cache_read_tokens: Option<i64>,
    pub(super) cache_write_tokens: Option<i64>,
    pub(super) total_cost: Option<f64>,
    pub(super) error_kind: Option<String>,
    pub(super) error_message: Option<String>,
    pub(super) duration_ms: Option<i64>,
}

fn serialized_name<T: Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .expect("failure kinds serialize")
        .as_str()
        .expect("failure kinds serialize as strings")
        .to_owned()
}

pub(super) fn extract_fields(event: &Event) -> Fields {
    match &event.payload {
        EventPayload::Triggered(p) => Fields {
            model: Some(p.config_snapshot.model.clone()),
            ..Default::default()
        },
        EventPayload::LlmRequest(p) => Fields {
            model: Some(p.model.clone()),
            ..Default::default()
        },
        // Cost now rides on the envelope (envelope-refactor plan
        // step 3); pull from envelope.cost when present so the
        // existing total_cost / input_tokens / output_tokens
        // columns stay populated.
        EventPayload::LlmResponse(p) => {
            let mut f = Fields {
                input_tokens: Some(p.usage.input_tokens as i64),
                output_tokens: Some(p.usage.output_tokens as i64),
                cache_read_tokens: Some(p.usage.cache_read_tokens as i64),
                cache_write_tokens: Some(p.usage.cache_write_tokens as i64),
                ..Default::default()
            };
            if let Some(cost) = &event.envelope.cost {
                f.model = Some(cost.model.clone());
                f.total_cost = Some(cost.total_cost);
            }
            f
        }
        // The summariser's own spend (#216): everything lives on
        // envelope.cost (the llm_response pattern), emitted under the
        // reserved `summary` agent id — `fq costs` reports it as its
        // own row with no changes to the cost queries.
        EventPayload::InvocationSummary(_) => {
            let mut f = Fields::default();
            if let Some(cost) = &event.envelope.cost {
                f.model = Some(cost.model.clone());
                f.input_tokens = Some(cost.input_tokens as i64);
                f.output_tokens = Some(cost.output_tokens as i64);
                f.cache_read_tokens = Some(cost.cache_read_tokens as i64);
                f.cache_write_tokens = Some(cost.cache_write_tokens as i64);
                f.total_cost = Some(cost.total_cost);
            }
            f
        }
        EventPayload::ToolCall(_) => Fields::default(),
        EventPayload::ToolDispatched(_) => Fields::default(),
        EventPayload::LlmDispatched(_) => Fields::default(),
        EventPayload::HostNotice(_) => Fields::default(),
        EventPayload::InvocationAmbiguous(_) => Fields::default(),
        EventPayload::InvocationArchived(_) => Fields::default(),
        EventPayload::InvocationArchiveAcked(_) => Fields::default(),
        EventPayload::ToolResult(p) => Fields {
            error_kind: p.error_kind.map(serialized_name),
            duration_ms: Some(p.duration_ms as i64),
            ..Default::default()
        },
        EventPayload::Completed(p) => Fields {
            total_cost: Some(p.total_cost),
            duration_ms: Some(p.total_duration_ms as i64),
            ..Default::default()
        },
        EventPayload::Failed(p) => Fields {
            error_kind: Some(serialized_name(p.error_kind)),
            error_message: Some(p.error_message.clone()),
            duration_ms: Some(p.partial_totals.total_duration_ms as i64),
            total_cost: Some(p.partial_totals.total_cost),
            ..Default::default()
        },
        // System events carry no agent metadata. The projection
        // still records them for visibility (useful for "when did
        // the daemon restart" queries), but every denormalised
        // column is NULL. WorkerHeartbeat never reaches this point —
        // `insert_event` drops it (operational signal, not data).
        EventPayload::SystemStartup(_)
        | EventPayload::SystemShutdown(_)
        | EventPayload::SystemTaskFailed(_)
        | EventPayload::SystemRecovery(_)
        | EventPayload::WorkerHeartbeat(_)
        | EventPayload::WorkerOrphaned(_)
        | EventPayload::McpServerLog(_)
        | EventPayload::InvocationOperatorRecovered(_)
        | EventPayload::InvocationOperatorResumed(_)
        // A type this binary cannot read: the envelope columns still
        // project, so the row records that something happened here.
        | EventPayload::Unknown => Fields::default(),
    }
}

pub(super) fn summary_kind_name(kind: crate::events::SummaryKind) -> &'static str {
    match kind {
        crate::events::SummaryKind::Start => "start",
        crate::events::SummaryKind::Progress => "progress",
        crate::events::SummaryKind::Outcome => "outcome",
    }
}
