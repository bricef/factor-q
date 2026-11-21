//! The `From<Row>` conversions from the stores' rows to the view types:
//! moved from `views.rs`, which the size cap holds at 800 production
//! lines, ahead of the invocation views growing the termination record
//! (issue 1070).

use super::*;
use crate::control_plane::projection::store::{
    CostBucketSummary, CostSummary, EventRow, FailureSummary, InvocationCostSummary,
    ModelCostSummary,
};
use crate::control_plane::store::{InvocationArchiveRow, OwnerRow, WorkerRow};
use crate::worker::store::{LlmDispatchRow, ToolDispatchRow};

impl From<WorkerRow> for WorkerView {
    fn from(r: WorkerRow) -> Self {
        WorkerView {
            worker_id: r.worker_id,
            host: r.host,
            registered_at_ms: r.registered_at,
            last_heartbeat_ms: r.last_heartbeat,
            status: r.status.as_str().to_string(),
            in_flight_count: 0,
        }
    }
}

impl From<OwnerRow> for InvocationSummaryView {
    fn from(r: OwnerRow) -> Self {
        InvocationSummaryView {
            invocation_id: r.invocation_id,
            agent_id: None,
            worker_id: r.worker_id,
            status: r.status.as_str().to_string(),
            assigned_at_ms: r.assigned_at,
            started_at_ms: r.assigned_at,
            archived: false,
            archive_state: r.archive_state.into(),
            summary: None,
        }
    }
}

impl From<InvocationArchiveRow> for ArchiveView {
    fn from(r: InvocationArchiveRow) -> Self {
        ArchiveView {
            invocation_id: r.invocation_id,
            agent_id: r.agent_id,
            final_phase: r.final_phase,
            started_at_ms: r.started_at,
            terminal_at_ms: r.terminal_at,
            archived_at_ms: r.archived_at,
        }
    }
}

impl From<EventRow> for EventView {
    fn from(r: EventRow) -> Self {
        EventView {
            event_id: r.event_id,
            timestamp: r.timestamp,
            agent_id: r.agent_id,
            invocation_id: r.invocation_id,
            event_type: r.event_type,
            model: r.model,
            total_cost: r.total_cost,
            error_kind: r.error_kind,
            error_message: r.error_message,
            duration_ms: r.duration_ms,
        }
    }
}

impl From<CostSummary> for CostView {
    fn from(r: CostSummary) -> Self {
        CostView {
            agent_id: r.agent_id,
            event_count: r.event_count,
            total_cost: r.total_cost,
            total_input_tokens: r.total_input_tokens,
            total_output_tokens: r.total_output_tokens,
            total_cache_read_tokens: r.total_cache_read_tokens,
            total_cache_write_tokens: r.total_cache_write_tokens,
            total_reasoning_tokens: r.total_reasoning_tokens,
            invocation_count: r.invocation_count,
            framework_cost: r.framework_cost,
        }
    }
}

impl From<InvocationCostSummary> for InvocationCostView {
    fn from(r: InvocationCostSummary) -> Self {
        InvocationCostView {
            started_at_ms: chrono::DateTime::parse_from_rfc3339(&r.first_timestamp)
                .map(|d| d.timestamp_millis())
                .unwrap_or(0),
            invocation_id: r.invocation_id,
            event_count: r.event_count,
            total_cost: r.total_cost,
            total_input_tokens: r.total_input_tokens,
            total_output_tokens: r.total_output_tokens,
            total_cache_read_tokens: r.total_cache_read_tokens,
            total_cache_write_tokens: r.total_cache_write_tokens,
            total_reasoning_tokens: r.total_reasoning_tokens,
        }
    }
}

impl From<ModelCostSummary> for ModelCostView {
    fn from(r: ModelCostSummary) -> Self {
        ModelCostView {
            model: r.model,
            event_count: r.event_count,
            total_cost: r.total_cost,
            total_input_tokens: r.total_input_tokens,
            total_output_tokens: r.total_output_tokens,
            total_cache_read_tokens: r.total_cache_read_tokens,
            total_cache_write_tokens: r.total_cache_write_tokens,
            total_reasoning_tokens: r.total_reasoning_tokens,
        }
    }
}

impl From<CostBucketSummary> for CostBucketView {
    fn from(r: CostBucketSummary) -> Self {
        CostBucketView {
            bucket: r.bucket,
            total_cost: r.total_cost,
        }
    }
}

impl From<FailureSummary> for FailureView {
    fn from(r: FailureSummary) -> Self {
        FailureView {
            error_kind: r.error_kind,
            count: r.count,
        }
    }
}

impl From<ToolDispatchRow> for ToolDispatchView {
    fn from(r: ToolDispatchRow) -> Self {
        ToolDispatchView {
            tool_call_id: r.tool_call_id,
            tool_name: r.tool_name,
            status: r.status.as_str().to_string(),
            is_error: r.is_error,
            intent_at_ms: r.intent_at,
            dispatched_at_ms: r.dispatched_at,
            completed_at_ms: r.completed_at,
        }
    }
}

impl From<LlmDispatchRow> for LlmDispatchView {
    fn from(r: LlmDispatchRow) -> Self {
        LlmDispatchView {
            request_id: r.request_id,
            model: r.model,
            status: r.status.as_str().to_string(),
            cost_usd: r.cost_usd,
            is_error: r.is_error,
            intent_at_ms: r.intent_at,
            dispatched_at_ms: r.dispatched_at,
            completed_at_ms: r.completed_at,
        }
    }
}
