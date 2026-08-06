//! How a flipped verb asks the daemon a question: dialling the edge
//! with the stored pairing, one authenticated call, and the two
//! composed reads the transcript needs.
//!
//! Split out of `operator_surface.rs` (#189), which is the daemon's
//! assembly point — where declarations meet their handlers — and had
//! been hosting the client's calling code as a lodger. The seam is the
//! one Phase 5 splits the binary along: everything here runs in `fq`,
//! everything there runs in `fqd`. Keeping the assembly file under its
//! size budget is what forced the issue, but the line was already the
//! right one.

use anyhow::Context;

use crate::cli::GlobalArgs;
use crate::connections::{edge_client, parse_fingerprint_hex, stored_connection};
use crate::event_atom::EventFilter;
use crate::operator_surface::TurnFilter;

/// Dial the configured daemon's edge with the stored pairing. One
/// handle per verb, not per call: a verb that asks more than one
/// question (`invocation transcript --follow` seeks the turn stream's
/// tail, reads the snapshot, then long-polls) pays for the TLS
/// handshake and the token exchange once, and every answer comes from
/// the same daemon incarnation.
pub(crate) async fn edge_client_for(global: &GlobalArgs) -> anyhow::Result<fq_edge::EdgeClient> {
    let config = global.resolve_config()?;
    let addr = config.edge.bind.clone();
    let entry = stored_connection(&addr)?;
    edge_client(
        &addr,
        parse_fingerprint_hex(&entry.fingerprint)?,
        &entry.token,
    )
    .await
}

/// One authenticated call on an open client: the outer error is
/// transport, the inner is the operation's own verdict — callers that
/// care (show's not-found path) match it, everyone else surfaces it.
pub(crate) async fn invoke_on(
    client: &fq_edge::EdgeClient,
    op: fq_ops::OpId,
    input: serde_json::Value,
) -> anyhow::Result<Result<serde_json::Value, fq_edge::wire::WireError>> {
    invoke_gated_on(client, op, input, None).await
}

/// [`invoke_on`], watermarked: `min_seq` holds the answer until this
/// daemon's fold has applied at least that sequence. It is the read
/// half of read-your-writes — the number comes from a command's
/// receipt (D4) — and it is a read-only argument: the edge refuses a
/// command that carries one.
pub(crate) async fn invoke_gated_on(
    client: &fq_edge::EdgeClient,
    op: fq_ops::OpId,
    input: serde_json::Value,
    min_seq: Option<u64>,
) -> anyhow::Result<Result<serde_json::Value, fq_edge::wire::WireError>> {
    let response = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op,
                version: 1,
                input,
                min_seq,
            },
        )
        .await
        .context("edge rpc failed")?;
    Ok(response.map(|r| r.output))
}

/// One authenticated edge call using the stored pairing for the
/// configured daemon — the single-question form: dial, ask, hang up.
pub(crate) async fn edge_invoke(
    global: &GlobalArgs,
    op: fq_ops::OpId,
    input: serde_json::Value,
) -> anyhow::Result<Result<serde_json::Value, fq_edge::wire::WireError>> {
    invoke_on(&edge_client_for(global).await?, op, input).await
}

/// The transcript wants the whole conversation, not a page of it.
/// `turn.list` pages at 200 by default, which would silently clip a
/// long run's tail; the daemon walks the invocation's stream either
/// way, so asking for everything costs the same scan and only makes
/// the answer complete.
const TRANSCRIPT_TURN_LIMIT: u32 = u32::MAX;

/// The transcript snapshot, over the edge: `turn.list`, rendered
/// through the turn→entry bridge. One question, one answer.
///
/// The opening prompt is a Turn like any other — folded from the
/// invocation's opening `llm.request`, which the runner publishes
/// before it calls the provider — so there is nothing left for the
/// transcript to compose. It used to ask `invocation.get` for the
/// prompt as well, on the mistaken belief that the prompt never became
/// an event; it always had.
///
/// `None` means "no turns recorded for this id", which is also what an
/// id the daemon has never heard of looks like. Both are the caller's
/// established not-found path, so they are not distinguished here.
pub(crate) async fn edge_transcript_snapshot(
    client: &fq_edge::EdgeClient,
    invocation_id: &str,
) -> anyhow::Result<Option<Vec<fq_runtime::transcript::TranscriptEntry>>> {
    use fq_edge::wire::WireError;

    let turns = match invoke_on(
        client,
        fq_ops::OpId::List(fq_ops::Domain::Turn),
        serde_json::to_value(TurnFilter {
            invocation_id: invocation_id.to_string(),
            limit: Some(TRANSCRIPT_TURN_LIMIT),
        })?,
    )
    .await?
    {
        Ok(value) => serde_json::from_value::<Vec<fq_runtime::turn::TurnState>>(value)?,
        Err(WireError::NotFound { .. }) => Vec::new(),
        Err(e) => anyhow::bail!("{e}"),
    };

    if turns.is_empty() {
        return Ok(None);
    }
    // Log order is chronological and `turn.list` answers in sequence
    // order, so the rendering needs no re-sort to reproduce the
    // WAL-backed timeline.
    Ok(Some(
        turns
            .iter()
            .map(fq_runtime::turn::TurnState::transcript_entry)
            .collect(),
    ))
}

/// Slack on a long poll's deadline: how much longer the caller waits
/// than the window it asked the daemon to hold. Covers the round trip
/// and the daemon's own scheduling under load — generous, because the
/// deadline is a backstop against a hung daemon, not the thing that
/// ends a poll.
const LONG_POLL_DEADLINE_SLACK: std::time::Duration = std::time::Duration::from_secs(10);

/// The RPC context for a long poll: patient enough for the wait it is
/// asking for.
///
/// tarpc's default deadline is a flat ten seconds, which is **shorter
/// than the window these calls ask the daemon to hold** (30s). A poll
/// that legitimately waits out its window is then abandoned by the
/// very client that asked for it, and the verb dies with `edge rpc
/// failed: DeadlineExceeded`.
///
/// That this was not obvious is worth recording: `event.stream` reads
/// the whole log, and the daemon heartbeats every
/// `DEFAULT_INTERVAL_MS` — exactly 10s — so an idle tail's poll was
/// ended by a heartbeat in a photo finish with the deadline, and lost
/// the race only under load. `turn.stream` has no such cover: it is
/// filtered to one agent's subject, so `invocation transcript
/// --follow` on a quiet invocation loses every time.
fn long_poll_context(max_wait_ms: u64) -> tarpc::context::Context {
    let mut ctx = tarpc::context::current();
    ctx.deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(max_wait_ms)
        + LONG_POLL_DEADLINE_SLACK;
    ctx
}

/// One long-poll batch of events from the edge. `from_seq = u64::MAX`
/// seeks the tail without consuming anything — the seam `fq events
/// tail` starts from, and the same cursor it resumes at, so a tail that
/// reconnects picks up exactly where it stopped rather than wherever
/// the broker happens to be (plan Phase 4, verb 11).
pub(crate) async fn next_event_batch(
    client: &fq_edge::EdgeClient,
    filter: &EventFilter,
    from_seq: u64,
    max_wait_ms: u64,
) -> anyhow::Result<Result<fq_edge::wire::StreamBatch, fq_edge::wire::WireError>> {
    client
        .rpc
        .next_batch(
            long_poll_context(max_wait_ms),
            fq_edge::NextBatchRequest {
                op: fq_ops::OpId::Stream(fq_ops::Domain::Event),
                version: 1,
                filter: serde_json::to_value(filter)?,
                from_seq,
                max_wait_ms,
            },
        )
        .await
        .context("edge rpc failed")
}

/// One long-poll batch of an invocation's turns from the edge.
/// `from_seq = u64::MAX` seeks the tail without consuming anything —
/// the gap-free seam `--follow` pins before it reads the snapshot.
pub(crate) async fn next_turn_batch(
    client: &fq_edge::EdgeClient,
    invocation_id: &str,
    from_seq: u64,
    max_wait_ms: u64,
) -> anyhow::Result<Result<fq_edge::wire::StreamBatch, fq_edge::wire::WireError>> {
    client
        .rpc
        .next_batch(
            long_poll_context(max_wait_ms),
            fq_edge::NextBatchRequest {
                op: fq_ops::OpId::Stream(fq_ops::Domain::Turn),
                version: 1,
                filter: serde_json::to_value(TurnFilter {
                    invocation_id: invocation_id.to_string(),
                    limit: None,
                })?,
                from_seq,
                max_wait_ms,
            },
        )
        .await
        .context("edge rpc failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant both long-polling verbs depend on, pinned where
    /// it cannot cost wall-clock time to check: a caller must be more
    /// patient than the wait it asks for. tarpc's default is a flat
    /// ten seconds, so this is the one thing that stops a 30-second
    /// poll from being abandoned at ten.
    #[test]
    fn a_long_poll_outlasts_the_wait_it_asks_for() {
        for max_wait_ms in [0, 30_000, 60_000] {
            let ctx = long_poll_context(max_wait_ms);
            let asked = std::time::Instant::now() + std::time::Duration::from_millis(max_wait_ms);
            assert!(
                ctx.deadline > asked,
                "a {max_wait_ms}ms poll must not be abandoned before it is answered"
            );
        }
        // And the default it replaces would not have been: this is
        // the regression, stated.
        assert!(
            tarpc::context::current().deadline
                < std::time::Instant::now() + std::time::Duration::from_millis(30_000),
            "tarpc's default deadline is shorter than a 30s poll — the bug this guards"
        );
    }
}
