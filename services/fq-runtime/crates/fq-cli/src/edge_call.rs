//! How a flipped verb asks the daemon a question: dialling the edge
//! with the **operator's** stored pairing, and the composed reads the
//! transcript needs.
//!
//! Split out of `operator_surface.rs` (#189), which is the daemon's
//! assembly point — where declarations meet their handlers — and had
//! been hosting the client's calling code as a lodger. The seam is the
//! one Phase 5 splits the binary along: everything here runs in `fq`,
//! everything there runs in `fqd`. Keeping the assembly file under its
//! size budget is what forced the issue, but the line was already the
//! right one.
//!
//! The envelope itself is no longer here. `invoke`/`next_batch` and
//! the long-poll deadline moved to [`fq_edge::EdgeClient`] when the
//! dashboard became the surface's second client — what remains is the
//! part that is genuinely the CLI's: resolving *this operator's*
//! configured daemon and pairing, and the filter shapes its verbs
//! send.

use crate::cli::GlobalArgs;
use crate::connections::{edge_client, stored_connection};
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
        fq_edge::parse_fingerprint_hex(&entry.fingerprint)?,
        &entry.token,
    )
    .await
}

/// One authenticated edge call using the stored pairing for the
/// configured daemon — the single-question form: dial, ask, hang up.
pub(crate) async fn edge_invoke(
    global: &GlobalArgs,
    op: fq_ops::OpId,
    input: serde_json::Value,
) -> anyhow::Result<Result<serde_json::Value, fq_edge::wire::WireError>> {
    edge_client_for(global).await?.invoke(op, input).await
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

    let turns = match client
        .invoke(
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
        .next_batch(
            fq_ops::OpId::Stream(fq_ops::Domain::Event),
            serde_json::to_value(filter)?,
            from_seq,
            max_wait_ms,
        )
        .await
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
        .next_batch(
            fq_ops::OpId::Stream(fq_ops::Domain::Turn),
            serde_json::to_value(TurnFilter {
                invocation_id: invocation_id.to_string(),
                limit: None,
            })?,
            from_seq,
            max_wait_ms,
        )
        .await
}
