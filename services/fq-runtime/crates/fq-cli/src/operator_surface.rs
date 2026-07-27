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

/// The typed input of `invocation.drop` on the wire. `--live` halting
/// stays on the control path until the Phase-4 drop flip.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct DropCommandInput {
    invocation_id: String,
    #[serde(default)]
    reason: Option<String>,
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
                    let detail = views
                        .invocation(
                            &key.invocation_id,
                            chrono::Utc::now().timestamp_millis(),
                            fq_runtime::control_plane::coordination_consumer::DEFAULT_STALE_THRESHOLD_MS,
                            fq_runtime::views::DEFAULT_LONG_DISPATCH_THRESHOLD_MS,
                        )
                        .await
                        .map_err(|e| WireError::Internal {
                            message: e.to_string(),
                        })?;
                    detail.ok_or_else(|| WireError::NotFound {
                        op: "invocation.get".into(),
                        message: format!("no invocation `{}`", key.invocation_id),
                    })
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
         gated read for read-your-writes. Halting live work stays on the \
         control path until the Phase-4 flip.",
    );
    registry
        .command::<DropCommandInput, _, _>(decl, move |input: DropCommandInput| {
            let bus = deps.bus.clone();
            let projection = deps.projection.clone();
            let control_plane = deps.control_plane.clone();
            async move {
                let result = fq_runtime::control_plane::operator::drop_invocation(
                    &bus,
                    &projection,
                    &control_plane,
                    &input.invocation_id,
                    input.reason.as_deref(),
                )
                .await
                .map_err(|e| match e {
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

/// One authenticated edge call using the stored pairing for the
/// configured daemon: the outer error is transport/credentials, the
/// inner is the operation's own verdict — callers that care (show's
/// not-found path) match it, everyone else surfaces it.
pub(crate) async fn edge_invoke(
    global: &GlobalArgs,
    op: fq_ops::OpId,
    input: serde_json::Value,
) -> anyhow::Result<Result<serde_json::Value, fq_edge::wire::WireError>> {
    let config = global.resolve_config()?;
    let addr = config.edge.bind.clone();
    let entry = stored_connection(&addr)?;
    let client = edge_client(
        &addr,
        parse_fingerprint_hex(&entry.fingerprint)?,
        &entry.token,
    )
    .await?;
    let response = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op,
                version: 1,
                input,
                min_seq: None,
            },
        )
        .await
        .context("edge rpc failed")?;
    Ok(response.map(|r| r.output))
}
