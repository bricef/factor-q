//! The operator surface (plan Phase 3): the real declarations bound
//! to Views-backed handlers — the daemon is where declarations meet
//! their implementations — plus the generic edge call the flipped
//! CLI verbs ride. Contract shapes that aren't runtime DTOs (keys,
//! filters) live here until 3e's codegen decision settles their
//! final home.

use std::sync::Arc;

use fq_runtime::views::Views;

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

/// Parse a `--status` filter into an `OwnerStatus`. Returns
/// `Err` on unknown values so the CLI exits with a clear
/// message rather than silently matching no rows.
pub(crate) fn parse_invocation_status_filter(
    s: &str,
) -> anyhow::Result<fq_runtime::control_plane::store::OwnerStatus> {
    use fq_runtime::control_plane::store::OwnerStatus;
    match s {
        "in_flight" => Ok(OwnerStatus::InFlight),
        "ambiguous" => Ok(OwnerStatus::Ambiguous),
        "completed" => Ok(OwnerStatus::Completed),
        "failed" => Ok(OwnerStatus::Failed),
        other => Err(anyhow::anyhow!(
            "unknown status filter `{other}` — try in_flight | ambiguous | completed | failed"
        )),
    }
}

/// Get identity for the Worker view.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct WorkerViewKey {
    pub(crate) worker_id: String,
}

/// Get identity for the Agent view.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct AgentViewKey {
    pub(crate) agent_id: String,
}

/// List selection for the Agent view. Empty, and declared anyway: a
/// registry is a directory of definitions the daemon holds entirely in
/// memory, so there is no narrowing worth a wire contract yet, and the
/// declaration is where a future one would appear.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct AgentListFilter {}

/// List selection for the Worker view — the typed, schema'd filter
/// (never a query language). `fq workers list` used to pull the whole
/// roster and sieve it in the client; the selection now travels with
/// the request and the view applies it to its index.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct WorkerListFilter {
    /// `alive` | `stale` | `shutdown`. Absent lists the whole roster.
    #[serde(default)]
    pub(crate) status: Option<String>,
}

/// What the operator surface's handlers reach for beyond [`Views`]:
/// the bus and writer stores commands write through, the runner a
/// command asks about liveness, and the live agent registry the Agent
/// view reads. Everything a *fold* can answer still comes from
/// `Views`; the split keeps that read path visibly read-only.
pub struct OperatorDeps {
    pub bus: fq_runtime::EventBus,
    pub projection: Arc<fq_runtime::control_plane::projection::ProjectionStore>,
    pub control_plane: Arc<fq_runtime::control_plane::store::ControlPlaneStore>,
    /// The reducer runner this daemon drives invocations with — the
    /// zero-lag liveness authority `invocation.drop` asks before it
    /// writes anything (#107). A command that can stop work needs the
    /// thing doing the work, not a projection of it.
    pub runner: Arc<fq_runtime::ReducerRunner<fq_runtime::Harness>>,
    /// The hot-swapped registry handle the dispatcher reads and `fq
    /// reload` replaces — the Agent view's source. Not a store and not
    /// a fold: agent definitions are configuration this daemon holds
    /// in memory, and the whole point of verb 9's flip is that the
    /// answer comes from what the daemon would actually run.
    pub agents: fq_runtime::SharedRegistry,
    /// What the machinery verbs command (plan Phase 4, verbs 3 and 4):
    /// the registry `control.reload` swaps, and the stop switch
    /// `control.down` throws. Grouped rather than flattened because
    /// these are the only fields a *command over the daemon itself*
    /// needs — everything above serves a resource.
    pub machinery: crate::control_commands::MachineryDeps,
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
    /// switch is opt-in, never implied. Defaults false, which
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

/// Build the daemon's operator registry: the Invocation and Worker
/// views served from [`Views`], the Agent view served from the live
/// registry, reads gated at the read horizon (every consumer feeding a
/// view's fold), the Turn, Event and DeadLetter atoms served from the
/// log, the Trigger atom served from its permanent projection record,
/// the commands — `invocation.drop`, `trigger.publish`,
/// `dead_letter.requeue`, and the two machinery verbs — each returning
/// a receipt, and the reports: the two Cost aggregates and the two
/// Control machinery reports (`control.doctor`, `control.status`).
/// Public for the operator-surface snapshot test.
pub fn operator_registry(
    views: Arc<Views>,
    horizon: fq_runtime::watermark::Horizon,
    min_seq_bound: std::time::Duration,
    deps: OperatorDeps,
) -> anyhow::Result<fq_edge::EdgeRegistry> {
    use fq_edge::wire::WireError;

    // Cloned up front: the registrations below each move their own
    // handles into 'static closures.
    let event_bus = deps.bus.clone();
    let event_views = views.clone();
    let dead_letter_bus = deps.bus.clone();
    let requeue_bus = deps.bus.clone();
    let requeue_projection = deps.projection.clone();
    let trigger_bus = deps.bus.clone();
    let trigger_views = views.clone();
    let turn_bus = deps.bus.clone();
    let turn_views = views.clone();
    let worker_views = views.clone();
    let cost_views = views.clone();
    let doctor_views = views.clone();
    let status_views = views.clone();
    let status_bus = deps.bus.clone();
    let status_registry = deps.agents.clone();
    let agent_registry = deps.agents.clone();
    let machinery = deps.machinery;

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

    register_worker_view(&mut registry, worker_views)?;
    register_agent_view(&mut registry, agent_registry)?;
    crate::event_atom::register_event_atom(&mut registry, event_bus, event_views)?;
    crate::dead_letter_atom::register_dead_letter_atom(&mut registry, dead_letter_bus)?;
    crate::dead_letter_requeue::register_dead_letter_requeue(
        &mut registry,
        requeue_bus,
        requeue_projection,
    )?;
    crate::trigger_command::register_trigger_surface(&mut registry, trigger_bus, trigger_views)?;
    crate::control_commands::register_control_commands(&mut registry, machinery)?;
    crate::cost_report::register_cost_reports(&mut registry, cost_views)?;
    crate::doctor_report::register_doctor_report(&mut registry, doctor_views)?;
    crate::status_report::register_status_report(
        &mut registry,
        status_views,
        status_bus,
        status_registry,
    )?;

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
                    // The drop event, named by the identity `event.get`
                    // takes — the same `event_id` its listing carries,
                    // so the receipt walks to the whole event.
                    atoms: vec![fq_ops::AtomRef {
                        domain: fq_ops::Domain::Event,
                        key: serde_json::json!({ "event_id": result.event_id }),
                    }],
                    // The position is still here, where being a log
                    // coordinate is the point: it is what a gated read
                    // passes as `min_seq` for read-your-writes.
                    watermarks: [(fq_ops::Domain::Event, result.event_seq)]
                        .into_iter()
                        .collect(),
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
        .atom::<TurnKey, fq_runtime::turn::TurnState, fq_runtime::turn::TurnState, TurnFilter, _, _, _, _, _, _>(
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

/// The Worker view (plan Phase 4, verbs 21/22): Get answers with the
/// fold — roster row plus the invocations the worker owns — and List
/// answers with the index rows, narrowed by the request's typed
/// filter. Its own function because `operator_registry` is an
/// assembly point, not a place for one resource's wiring to live.
fn register_worker_view(
    registry: &mut fq_edge::EdgeRegistry,
    views: Arc<Views>,
) -> anyhow::Result<()> {
    use fq_edge::wire::WireError;

    let worker_get_views = views.clone();
    let worker_list_views = views;

    let decl = fq_ops::View::new::<
        WorkerViewKey,
        fq_runtime::views::WorkerDetailView,
        fq_runtime::views::WorkerView,
        WorkerListFilter,
    >(
        fq_ops::Domain::Worker,
        "A worker: the fold of its registration, heartbeats and ownership.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "Get answers with the roster row plus every invocation the worker \
         owns; List answers with the roster rows alone. Neither derives a \
         heartbeat age — the fold stays wall-clock-free, so a reader that \
         wants an age computes it from `last_heartbeat_ms`.",
    );
    registry
        .view::<WorkerViewKey, fq_runtime::views::WorkerDetailView, fq_runtime::views::WorkerView, WorkerListFilter, _, _, _, _>(
            decl,
            move |key: WorkerViewKey| {
                let views = worker_get_views.clone();
                async move {
                    views
                        .worker(&key.worker_id)
                        .await
                        .map_err(|e| WireError::Internal {
                            message: e.to_string(),
                        })?
                        .ok_or_else(|| WireError::NotFound {
                            op: "worker.get".into(),
                            message: format!("no worker `{}`", key.worker_id),
                        })
                }
            },
            move |filter: WorkerListFilter| {
                let views = worker_list_views.clone();
                async move {
                    let status = filter
                        .status
                        .as_deref()
                        .map(parse_worker_status_filter)
                        .transpose()?;
                    let roster = views.workers().await.map_err(|e| WireError::Internal {
                        message: e.to_string(),
                    })?;
                    // Applied here, not in the client: a filtered list
                    // costs the caller only the rows it asked for.
                    Ok(roster
                        .into_iter()
                        .filter(|w| status.is_none_or(|want| w.status == want.as_str()))
                        .collect())
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;

    Ok(())
}

/// The Worker view's status filter, validated at the edge: an
/// unknown value is a verdict on the request, so it comes back as
/// `InvalidInput` naming the accepted set rather than as an empty
/// list the caller would read as "no such workers".
fn parse_worker_status_filter(
    s: &str,
) -> Result<fq_runtime::control_plane::store::WorkerStatus, fq_edge::wire::WireError> {
    fq_runtime::control_plane::store::WorkerStatus::parse(s).ok_or_else(|| {
        fq_edge::wire::WireError::InvalidInput {
            op: "worker.list".into(),
            message: format!("unknown status filter `{s}` — try alive | stale | shutdown"),
        }
    })
}

/// The Agent view (plan Phase 4, verb 9): Get answers with one
/// definition in full, List with the registry snapshot's index — the
/// loaded definitions and the files that failed to load.
///
/// Its source is the daemon's `SharedRegistry`, the same handle the
/// dispatcher reads and `fq reload` swaps, which is the entire point:
/// `fq agent list` used to read the caller's own disk and could
/// disagree with what the daemon would actually run. The read gate
/// applies as it does to every view, but a registry is not a fold of
/// atoms — there is no sequence to wait for, and a caller passing
/// `min_seq` is waiting on the projection horizon, not on this answer.
fn register_agent_view(
    registry: &mut fq_edge::EdgeRegistry,
    agents: fq_runtime::SharedRegistry,
) -> anyhow::Result<()> {
    use fq_edge::wire::WireError;
    use fq_runtime::agent_view::{AgentDetailView, AgentEntryView};

    let agent_get = agents.clone();
    let agent_list = agents;

    let decl = fq_ops::View::new::<AgentViewKey, AgentDetailView, AgentEntryView, AgentListFilter>(
        fq_ops::Domain::Agent,
        "An agent definition, as the daemon's live registry holds it.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "The answer is what this daemon would run right now — the registry `fq reload` \
         swaps, not the definitions on the caller's disk. Get answers with one definition \
         in full, including its system prompt. List answers with one row per definition \
         file the registry knows: the agents it loaded, in id order, then the files it \
         rejected, because a definition that failed to parse is the row an operator most \
         needs to see and has no agent id to be listed under.",
    );
    registry
        .view::<AgentViewKey, AgentDetailView, AgentEntryView, AgentListFilter, _, _, _, _>(
            decl,
            move |key: AgentViewKey| {
                let agents = agent_get.clone();
                async move {
                    // Clone the inner Arc out of the lock so the wire
                    // work never holds it — the dispatcher's discipline.
                    let registry = agents.read().await.clone();
                    // An id the validator rejects cannot be in the
                    // registry, so it is not found rather than invalid.
                    let loaded = fq_runtime::AgentId::new(&key.agent_id)
                        .ok()
                        .and_then(|id| registry.get_loaded(&id).map(AgentDetailView::from_loaded));
                    loaded.ok_or_else(|| WireError::NotFound {
                        op: "agent.get".into(),
                        message: format!("no agent `{}` in the daemon's registry", key.agent_id),
                    })
                }
            },
            move |_filter: AgentListFilter| {
                let agents = agent_list.clone();
                async move {
                    let registry = agents.read().await.clone();
                    Ok(AgentEntryView::index(&registry))
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;

    Ok(())
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
    let subject = format!("fq.agent.{agent}.>");
    // The scan's end is the last sequence *this agent's subject*
    // matches, not the stream's last sequence: the walk below only
    // ever sees matching messages, so a stream tip belonging to
    // another agent (or to a heartbeat) is a sequence it would wait
    // for forever. Found while building the Event atom, which has the
    // same shape and would have had the same hang.
    let tip = bus
        .last_event_seq_matching(&subject)
        .await
        .map_err(internal)?;
    if tip == 0 {
        return Ok(Vec::new());
    }
    let limit = filter.limit.unwrap_or(TURN_LIST_DEFAULT_LIMIT) as usize;
    let mut events = bus.events_from(&subject, 1).await.map_err(internal)?;
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
