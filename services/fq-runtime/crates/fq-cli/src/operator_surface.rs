//! The operator surface (plan Phase 3): the real declarations bound
//! to Views-backed handlers — the daemon is where declarations meet
//! their implementations — plus the generic edge call the flipped
//! CLI verbs ride. Contract shapes that aren't runtime DTOs (keys,
//! filters) live here until 3e's codegen decision settles their
//! final home.

use super::*;

// ---------------------------------------------------------------------
// The operator surface (plan Phase 3): the real declarations, bound
// to Views-backed handlers — assembled here because the daemon is
// where declarations meet their implementations. The contract shapes
// that aren't runtime DTOs (keys, filters) live beside the assembly
// until 3e's codegen decision settles their final home.
// ---------------------------------------------------------------------

/// Get identity for the Invocation view.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct InvocationViewKey {
    pub(crate) invocation_id: String,
    /// Ask for the invocation's opening prompt alongside the fold.
    ///
    /// Not identity — the id alone names the invocation — but a
    /// declaration of what the reader will do with the answer. The
    /// prompt is the one unbounded field on the view
    /// (`InvocationDetailView::prompt`), so a reader that renders the
    /// conversation says so and pays for it, and a reader that wants
    /// the fold is not charged an agent's whole system prompt on every
    /// `invocation show`. Defaults false, which is also what every
    /// peer that predates the field sends.
    #[serde(default)]
    pub(crate) with_prompt: bool,
}

/// List selection for the Invocation view — the typed, schema'd
/// filter (never a query language).
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct InvocationListFilter {
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) include_archived: bool,
    #[serde(default = "default_invocation_list_limit")]
    pub(crate) limit: i64,
}

fn default_invocation_list_limit() -> i64 {
    50
}

/// What the operator surface's command handlers write through: the
/// daemon's bus and writer stores. Reads come from [`Views`]; the
/// split keeps the read path visibly read-only.
pub struct OperatorDeps {
    pub bus: fq_runtime::EventBus,
    pub projection: Arc<fq_runtime::control_plane::projection::ProjectionStore>,
    pub control_plane: Arc<fq_runtime::control_plane::store::ControlPlaneStore>,
    /// The reducer runner this daemon drives invocations with — the
    /// zero-lag liveness authority `invocation.drop` asks before it
    /// writes anything (#107). A command that can stop work needs the
    /// thing doing the work, not a projection of it.
    pub runner: Arc<fq_runtime::ReducerRunner<fq_runtime::Harness>>,
}

/// Get identity for a Turn: its event-log sequence.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct TurnKey {
    seq: u64,
}

/// List/Stream selection for Turns — full payloads by default; an
/// `abbreviate` option waits for a consumer that wants it (P11).
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct TurnFilter {
    pub(crate) invocation_id: String,
    #[serde(default)]
    pub(crate) limit: Option<u32>,
}

/// The typed input of `invocation.drop` on the wire.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct DropCommandInput {
    invocation_id: String,
    #[serde(default)]
    reason: Option<String>,
    /// Halt the invocation first if this daemon is actively driving
    /// it. Without it, live work is refused outright — the kill
    /// switch is opt-in, never implied (#107). Defaults false, which
    /// is also what every peer that predates the field sends.
    #[serde(default)]
    live: bool,
}

/// The live-drop precondition, daemon-side (plan Phase 4, verb 18):
/// the runner that would be driving this invocation is the only
/// zero-lag authority on whether it *is*, so the check and the halt
/// happen here — in the same handler that then writes the terminal
/// event, leaving no window between deciding and acting.
///
/// Returns **the agent the halted invocation belongs to**, when this
/// daemon was driving it. That is not a convenience: the runner is the
/// same authority that just justified stopping real work, and it is
/// ahead of every durable record (the projection is folded
/// asynchronously; nothing on the dispatch path writes an owner row).
/// Carrying its answer forward is what makes the drop's resolution
/// infallible whenever the halt was armed — so a call can never stop an
/// invocation and then report it unknown (#107).
///
/// Refusals are `InvalidInput`: they are verdicts on this request (a
/// bare drop of live work), not on the invocation's existence. The
/// operator reads the message verbatim, so each one names its remedy.
/// Every refusal here precedes the arm, so a refused drop stops nothing.
fn arm_drop_halt(
    runner: &fq_runtime::ReducerRunner<fq_runtime::Harness>,
    input: &DropCommandInput,
) -> Result<Option<fq_runtime::AgentId>, fq_edge::wire::WireError> {
    let refuse = |message: String| fq_edge::wire::WireError::InvalidInput {
        op: "invocation.drop".into(),
        message,
    };
    let Ok(id) = uuid::Uuid::parse_str(&input.invocation_id) else {
        return Err(refuse(format!(
            "invalid invocation id `{}`",
            input.invocation_id
        )));
    };
    let Some(agent) = runner.active_agent(&id) else {
        return Ok(None);
    };
    if !input.live {
        return Err(refuse(format!(
            "invocation {id} is currently running; use --live to halt and drop it"
        )));
    }
    // The arm can lose to the invocation finishing between the
    // liveness check and here. Report that rather than publishing
    // a terminal event: the operator-recovered handler
    // deliberately overrides any prior status, so a run that
    // completed on its own would be archived as `failed`.
    if !runner.request_halt(id) {
        return Err(refuse(format!(
            "invocation {id} finished before the halt could be armed; \
             nothing dropped — re-check `fq invocation show {id}`"
        )));
    }
    Ok(Some(agent))
}

/// Build the daemon's operator registry: the Invocation view served
/// from [`Views`], reads gated at the read horizon (every consumer
/// feeding the view's fold), and `invocation.drop` returning a
/// receipt. Public for the operator-surface snapshot test.
pub fn operator_registry(
    views: Arc<Views>,
    horizon: fq_runtime::watermark::Horizon,
    min_seq_bound: std::time::Duration,
    deps: OperatorDeps,
) -> anyhow::Result<fq_edge::EdgeRegistry> {
    use fq_edge::wire::WireError;

    // Cloned up front: the registrations below each move their own
    // handles into 'static closures.
    let turn_bus = deps.bus.clone();
    let turn_views = views.clone();

    let mut registry = fq_edge::EdgeRegistry::new().with_read_gate(Arc::new(move |min_seq| {
        let horizon = horizon.clone();
        Box::pin(async move {
            horizon
                .wait_for(min_seq, min_seq_bound)
                .await
                .map(|_| ())
                .map_err(|e| match e {
                    fq_runtime::watermark::WatermarkError::Lag { applied, .. }
                    | fq_runtime::watermark::WatermarkError::Stopped { applied, .. } => applied,
                })
        })
    }));

    let decl = fq_ops::View::new::<
        InvocationViewKey,
        fq_runtime::views::InvocationDetailView,
        fq_runtime::views::InvocationSummaryView,
        InvocationListFilter,
    >(
        fq_ops::Domain::Invocation,
        "An agent invocation: the fold of its lifecycle events.",
        fq_ops::Stability::Experimental,
    );

    let get_views = views.clone();
    registry
        .view::<InvocationViewKey, fq_runtime::views::InvocationDetailView, fq_runtime::views::InvocationSummaryView, InvocationListFilter, _, _, _, _>(
            decl,
            move |key: InvocationViewKey| {
                let views = get_views.clone();
                async move {
                    let internal = |e: fq_runtime::views::ViewsError| WireError::Internal {
                        message: e.to_string(),
                    };
                    let detail = views
                        .invocation(
                            &key.invocation_id,
                            chrono::Utc::now().timestamp_millis(),
                            fq_runtime::control_plane::coordination_consumer::DEFAULT_STALE_THRESHOLD_MS,
                            fq_runtime::views::DEFAULT_LONG_DISPATCH_THRESHOLD_MS,
                        )
                        .await
                        .map_err(internal)?;
                    let mut detail = detail.ok_or_else(|| WireError::NotFound {
                        op: "invocation.get".into(),
                        message: format!("no invocation `{}`", key.invocation_id),
                    })?;
                    // The prompt is composed here rather than inside
                    // the fold: it is the only opt-in field, and the
                    // reader's request is what decides whether the
                    // payload read happens at all.
                    if key.with_prompt {
                        detail.prompt = views
                            .invocation_prompt(&key.invocation_id)
                            .await
                            .map_err(internal)?;
                    }
                    Ok(detail)
                }
            },
            move |filter: InvocationListFilter| {
                let views = views.clone();
                async move {
                    let status = filter
                        .status
                        .as_deref()
                        .map(parse_invocation_status_filter)
                        .transpose()
                        .map_err(|e| WireError::InvalidInput {
                            op: "invocation.list".into(),
                            message: e.to_string(),
                        })?;
                    views
                        .invocation_index(status, filter.include_archived, filter.limit)
                        .await
                        .map_err(|e| WireError::Internal {
                            message: e.to_string(),
                        })
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;

    let decl = fq_ops::Command::new::<DropCommandInput>(
        fq_ops::Invocation::Drop,
        fq_ops::Authority {
            verb: fq_ops::Verb::Write,
            scope: fq_ops::Domain::Invocation,
        },
        "Drop an invocation: archive it as failed and release its owner.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "Returns a receipt naming the drop event's sequence — feed it to a \
         gated read for read-your-writes. Refused on an invocation this \
         daemon is actively driving unless `live` is set, which halts it at \
         its next step boundary (in-flight tools finish) before the drop.",
    );
    registry
        .command::<DropCommandInput, _, _>(decl, move |input: DropCommandInput| {
            let bus = deps.bus.clone();
            let projection = deps.projection.clone();
            let control_plane = deps.control_plane.clone();
            let runner = deps.runner.clone();
            async move {
                // Liveness first: a refused drop must leave no trace, so
                // nothing is published until the halt is armed (or the
                // invocation is known idle). The arm hands back who the
                // halted invocation belongs to, and the write below
                // resolves from that rather than from a projection this
                // daemon may be ahead of — an armed halt and a NotFound
                // can never be the same answer (#107).
                let driving_agent = arm_drop_halt(&runner, &input)?;
                // The edge implementing an op IS the runtime boundary, not a
                // call point to flip.
                // allow-runtime-internals: daemon-side command handler.
                let result = fq_runtime::control_plane::operator::drop_invocation(
                    &bus,
                    &projection,
                    &control_plane,
                    &input.invocation_id,
                    input.reason.as_deref(),
                    driving_agent.as_ref(),
                )
                .await
                .map_err(|e| match e {
                    // allow-runtime-internals: daemon-side error mapping, same handler.
                    fq_runtime::control_plane::operator::DropError::UnknownInvocation(id) => {
                        WireError::NotFound {
                            op: "invocation.drop".into(),
                            message: format!("no invocation `{id}`"),
                        }
                    }
                    other => WireError::InvalidInput {
                        op: "invocation.drop".into(),
                        message: other.to_string(),
                    },
                })?;
                Ok(fq_ops::Receipt {
                    atoms: vec![fq_ops::AtomRef {
                        domain: fq_ops::Domain::Event,
                        seq: result.event_seq,
                    }],
                })
            }
        })
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;

    let decl = fq_ops::Atom::new::<TurnKey, fq_runtime::turn::TurnState, TurnFilter>(
        fq_ops::Domain::Turn,
        "One action within a Round: an assistant output or a tool result.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "Event-log-backed: `seq` is the stream sequence — the same \
         cursor receipts and min_seq gates speak. Stream long-polls \
         via next_batch; from_seq = u64::MAX seeks the tail.",
    );
    let get_bus = turn_bus.clone();
    let list_bus = turn_bus.clone();
    let list_views = turn_views.clone();
    let stream_views = turn_views;
    registry
        .atom::<TurnKey, fq_runtime::turn::TurnState, TurnFilter, _, _, _, _, _, _>(
            decl,
            move |key: TurnKey| {
                let bus = get_bus.clone();
                async move { turn_at(&bus, key.seq).await }
            },
            move |filter: TurnFilter| {
                let bus = list_bus.clone();
                let views = list_views.clone();
                async move {
                    let agent = agent_for_turns(&views, &filter.invocation_id).await?;
                    list_turns(&bus, &agent, &filter).await
                }
            },
            move |filter: TurnFilter, from_seq, max_wait_ms| {
                let bus = turn_bus.clone();
                let views = stream_views.clone();
                async move {
                    let agent = agent_for_turns(&views, &filter.invocation_id).await?;
                    stream_turns(&bus, &agent, &filter.invocation_id, from_seq, max_wait_ms).await
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;

    Ok(registry)
}

/// Cap on one stream batch and one list page.
const TURN_BATCH_CAP: usize = 64;
const TURN_LIST_DEFAULT_LIMIT: u32 = 200;
/// Ceiling on a `next_batch` long poll, whatever the caller asks.
const TURN_MAX_WAIT_CEILING_MS: u64 = 60_000;

async fn agent_for_turns(
    views: &Views,
    invocation_id: &str,
) -> Result<String, fq_edge::wire::WireError> {
    views
        .agent_id_for_invocation(invocation_id)
        .await
        .map_err(|e| fq_edge::wire::WireError::Internal {
            message: e.to_string(),
        })?
        .ok_or_else(|| fq_edge::wire::WireError::NotFound {
            op: "turn.list".into(),
            message: format!("no invocation `{invocation_id}`"),
        })
}

/// Get one turn by log sequence: read the event at `seq` (if it is an
/// agent event) and fold it alone — a lone tool result renders from
/// its restated `tool_name`, with `parameters` null and no
/// `initiating_turn` (the join needs the window).
async fn turn_at(
    bus: &fq_runtime::EventBus,
    seq: u64,
) -> Result<fq_runtime::turn::TurnState, fq_edge::wire::WireError> {
    use futures::StreamExt;
    let not_found = || fq_edge::wire::WireError::NotFound {
        op: "turn.get".into(),
        message: format!("no turn at sequence {seq}"),
    };
    let mut events = bus.events_from("fq.agent.>", seq).await.map_err(|e| {
        fq_edge::wire::WireError::Internal {
            message: e.to_string(),
        }
    })?;
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), events.next())
        .await
        .map_err(|_| not_found())?
        .ok_or_else(not_found)?
        .map_err(|e| fq_edge::wire::WireError::Internal {
            message: e.to_string(),
        })?;
    let (got_seq, event) = first;
    if got_seq != seq {
        return Err(not_found());
    }
    fq_runtime::turn::TurnFold::new()
        .apply(got_seq, &event)
        .ok_or_else(not_found)
}

/// List an invocation's turns from the log, full payloads, bounded by
/// the stream tip observed at entry.
async fn list_turns(
    bus: &fq_runtime::EventBus,
    agent: &str,
    filter: &TurnFilter,
) -> Result<Vec<fq_runtime::turn::TurnState>, fq_edge::wire::WireError> {
    use futures::StreamExt;
    let internal = |e: fq_runtime::bus::BusError| fq_edge::wire::WireError::Internal {
        message: e.to_string(),
    };
    let tip = bus.last_event_seq().await.map_err(internal)?;
    if tip == 0 {
        return Ok(Vec::new());
    }
    let limit = filter.limit.unwrap_or(TURN_LIST_DEFAULT_LIMIT) as usize;
    let mut events = bus
        .events_from(&format!("fq.agent.{agent}.>"), 1)
        .await
        .map_err(internal)?;
    let mut fold = fq_runtime::turn::TurnFold::new();
    let mut turns = Vec::new();
    while let Some(next) = events.next().await {
        let (seq, event) = next.map_err(internal)?;
        if event.envelope.invocation_id.to_string() == filter.invocation_id
            && let Some(turn) = fold.apply(seq, &event)
        {
            turns.push(turn);
            if turns.len() >= limit {
                break;
            }
        } else {
            // Non-matching events still feed the fold's join window.
            let _ = fold.apply(seq, &event);
        }
        if seq >= tip {
            break;
        }
    }
    Ok(turns)
}

/// One long-poll batch of an invocation's turns at or after
/// `from_seq`; `u64::MAX` seeks the tail. The cursor advances past
/// non-matching events too, so an idle poll still makes progress.
async fn stream_turns(
    bus: &fq_runtime::EventBus,
    agent: &str,
    invocation_id: &str,
    from_seq: u64,
    max_wait_ms: u64,
) -> Result<fq_edge::wire::StreamBatch, fq_edge::wire::WireError> {
    use futures::StreamExt;
    let internal = |e: fq_runtime::bus::BusError| fq_edge::wire::WireError::Internal {
        message: e.to_string(),
    };
    let from_seq = if from_seq == u64::MAX {
        bus.last_event_seq().await.map_err(internal)? + 1
    } else {
        from_seq
    };
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_millis(max_wait_ms.min(TURN_MAX_WAIT_CEILING_MS));
    let mut events = bus
        .events_from(&format!("fq.agent.{agent}.>"), from_seq)
        .await
        .map_err(internal)?;
    let mut fold = fq_runtime::turn::TurnFold::new();
    let mut items = Vec::new();
    let mut next_from_seq = from_seq;
    loop {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or(std::time::Duration::ZERO);
        // Once something is in hand, only drain what's immediately
        // ready; before that, wait out the long poll.
        let wait = if items.is_empty() {
            remaining
        } else {
            std::time::Duration::from_millis(10)
        };
        let next = match tokio::time::timeout(wait, events.next()).await {
            Ok(Some(next)) => next.map_err(internal)?,
            Ok(None) | Err(_) => break,
        };
        let (seq, event) = next;
        next_from_seq = seq + 1;
        let turn = fold.apply(seq, &event);
        if event.envelope.invocation_id.to_string() == invocation_id
            && let Some(turn) = turn
        {
            let item =
                serde_json::to_value(&turn).map_err(|e| fq_edge::wire::WireError::Internal {
                    message: e.to_string(),
                })?;
            items.push(fq_edge::wire::StreamItem { seq, item });
            if items.len() >= TURN_BATCH_CAP {
                break;
            }
        }
        if items.is_empty() && remaining.is_zero() {
            break;
        }
    }
    Ok(fq_edge::wire::StreamBatch {
        items,
        next_from_seq,
    })
}

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
