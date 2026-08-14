//! Ordering and regrouping, checked without a broker.
//!
//! These moved with their functions out of `runner/tests.rs` (#78).
//! They were always the pure end of that file — no NATS, no store, no
//! fixtures — and they run in microseconds.

use super::*;
use crate::events::{StopReason, TokenUsage};
use crate::worker::reducer::types::ModelResponse;

#[test]
fn sequence_order_preserves_tool_batch_boundaries_across_timestamp_ties() {
    let tool = |id: &str| {
        CapabilityResult::ToolResult(ToolCallResult {
            tool_call_id: crate::events::ToolCallId::new(id.to_string()).unwrap(),
            output: String::new(),
            is_error: false,
            error_kind: None,
            duration_ms: 0,
        })
    };
    let model = CapabilityResult::ModelResult(ModelResponse {
        content: Some("next".to_string()),
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
    });
    let ordered = vec![
        (replay_sort_key(Some(1), Some(42)), tool("first")),
        (replay_sort_key(Some(2), Some(42)), model),
        (replay_sort_key(Some(3), Some(42)), tool("second")),
    ];
    let replay = coalesce_tool_results(ordered);
    assert_eq!(replay.len(), 3);
    assert!(matches!(replay[0], CapabilityResult::ToolResult(_)));
    assert!(matches!(replay[1], CapabilityResult::ModelResult(_)));
    assert!(matches!(replay[2], CapabilityResult::ToolResult(_)));
}

/// Build a tagged capability so order assertions can identify rows
/// after sorting: the tool_call_id carries the tag.
fn tagged(tag: &str) -> CapabilityResult {
    CapabilityResult::ToolResult(ToolCallResult {
        tool_call_id: crate::events::ToolCallId::new(tag.to_string()).unwrap(),
        output: String::new(),
        is_error: false,
        error_kind: None,
        duration_ms: 0,
    })
}

fn tags(sorted: &[((Option<i64>, i64), CapabilityResult)]) -> Vec<String> {
    sorted
        .iter()
        .map(|(_, c)| match c {
            CapabilityResult::ToolResult(r) => r.tool_call_id.as_str().to_string(),
            other => panic!("expected tagged ToolResult, got {other:?}"),
        })
        .collect()
}

/// Fully-sequenced WALs total-order by seq in both directions,
/// through the production sort (not the tuple's natural `Ord`).
#[test]
fn replay_order_uses_seq_when_fully_sequenced() {
    let same_ms = 42;
    let mut llm_completed_first = vec![
        (replay_sort_key(Some(2), Some(same_ms)), tagged("second")),
        (replay_sort_key(Some(1), Some(same_ms)), tagged("first")),
    ];
    sort_into_replay_order(&mut llm_completed_first);
    assert_eq!(tags(&llm_completed_first), ["first", "second"]);

    let mut tool_completed_first = vec![
        (replay_sort_key(Some(1), Some(same_ms)), tagged("first")),
        (replay_sort_key(Some(2), Some(same_ms)), tagged("second")),
    ];
    sort_into_replay_order(&mut tool_completed_first);
    assert_eq!(tags(&tool_completed_first), ["first", "second"]);
}

/// A WAL spanning the v8→v9 migration falls back to timestamp
/// chronology for the whole list. This triple is the non-total-order
/// regression: under a pairwise seq/timestamp comparator it forms a
/// cycle (A<C by seq, B<A and C<B by timestamp) that `sort_by` may
/// panic on; the list-wide decision must order it by timestamp
/// without panicking.
#[test]
fn replay_order_falls_back_to_timestamps_when_legacy_rows_participate() {
    let mut mixed = vec![
        (replay_sort_key(Some(1), Some(100)), tagged("a-seq1-ts100")),
        (replay_sort_key(None, Some(50)), tagged("b-legacy-ts50")),
        (replay_sort_key(Some(2), Some(10)), tagged("c-seq2-ts10")),
    ];
    sort_into_replay_order(&mut mixed);
    assert_eq!(
        tags(&mixed),
        ["c-seq2-ts10", "b-legacy-ts50", "a-seq1-ts100"]
    );

    // Within a tied millisecond, sequenced rows order by seq and
    // precede legacy rows; pure-legacy ties keep insertion order
    // (the pre-v9 behaviour) via the stable sort.
    let mut tie = vec![
        (replay_sort_key(None, Some(42)), tagged("legacy-first-in")),
        (replay_sort_key(Some(2), Some(42)), tagged("seq2")),
        (replay_sort_key(None, Some(42)), tagged("legacy-second-in")),
        (replay_sort_key(Some(1), Some(42)), tagged("seq1")),
    ];
    sort_into_replay_order(&mut tie);
    assert_eq!(
        tags(&tie),
        ["seq1", "seq2", "legacy-first-in", "legacy-second-in"]
    );
}
