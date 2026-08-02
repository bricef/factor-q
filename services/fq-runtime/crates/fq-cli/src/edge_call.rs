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

use super::*;
use crate::operator_surface::{InvocationViewKey, TurnFilter};

/// Dial the configured daemon's edge with the stored pairing. One
/// handle per verb, not per call: a verb that asks two questions
/// (`invocation transcript` reads the prompt and the turns) pays for
/// the TLS handshake and the token exchange once, and both answers come
/// from the same daemon incarnation.
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

/// The transcript snapshot, over the edge: the invocation's turns
/// (`turn.list`) rendered through the turn→entry bridge, behind the
/// opening prompt from the Invocation view (`invocation.get`). A
/// transcript is a rendering composed over turns *plus* the
/// invocation's prompt — a prompt is not an action within a Round, so
/// it comes from the view, not the atom.
///
/// `None` means "nothing recorded for this id": no prompt and no
/// turns, which is also what an id the daemon has never heard of looks
/// like. Both cases are the caller's established not-found path, so
/// they are not distinguished here.
pub(crate) async fn edge_transcript_snapshot(
    client: &fq_edge::EdgeClient,
    invocation_id: &str,
) -> anyhow::Result<Option<Vec<fq_runtime::transcript::TranscriptEntry>>> {
    use fq_edge::wire::WireError;

    let prompt = match invoke_on(
        client,
        fq_ops::OpId::Get(fq_ops::Domain::Invocation),
        serde_json::to_value(InvocationViewKey {
            invocation_id: invocation_id.to_string(),
            with_prompt: true,
        })?,
    )
    .await?
    {
        Ok(value) => {
            serde_json::from_value::<fq_runtime::views::InvocationDetailView>(value)?.prompt
        }
        Err(WireError::NotFound { .. }) => None,
        Err(e) => anyhow::bail!("{e}"),
    };

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

    if prompt.is_none() && turns.is_empty() {
        return Ok(None);
    }
    // Log order is chronological, and the prompt precedes the first
    // turn by construction — no re-sort is needed to reproduce the
    // WAL-backed timeline.
    let mut entries = Vec::with_capacity(turns.len() + 1);
    entries.extend(prompt.map(fq_runtime::transcript::TranscriptPrompt::into_entry));
    entries.extend(
        turns
            .iter()
            .map(fq_runtime::turn::TurnState::transcript_entry),
    );
    Ok(Some(entries))
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
            tarpc::context::current(),
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
