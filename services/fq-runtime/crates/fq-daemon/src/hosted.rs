//! Running: the tasks the daemon hosts, and how it stops them.
//!
//! Split from `daemon.rs`, which is now assembly only. The seam is
//! where it is because almost every task handle is created here,
//! awaited here, and stopped here, so the boundary carries what was
//! *assembled* rather than what is running. `resume_handles` is the
//! one exception — startup recovery spawns before this module is
//! reached, so those handles ride across in `Assembled` and are joined
//! with the rest at the end.
//!
//! That is also why spawn and shutdown are one module rather than two:
//! separating them would mean handing eleven handles and their
//! shutdown senders across a function boundary, which is a worse shape
//! than the length it fixes. The *ordering* of the shutdown is its own
//! concern and does live next door, in `hosted/teardown.rs`.
//!
//! `run_hosted` has two phases and they must stay in this order:
//! everything fallible first, then everything spawned. A marker in the
//! body says where the line is.
//!
//! The order matters because a `?` after the first spawn returns
//! straight out of the function and skips the teardown at the end of
//! this file: no `system.shutdown` is published, and the MCP children
//! take the abrupt drop-guard kill instead of a graceful stop. The
//! tasks themselves do stop — but only as a side effect of their
//! shutdown senders being dropped as the locals unwind, which is not
//! the same thing.
//!
//! That is not hypothetical. An `edge: failed to bind` on a port a
//! previous daemon still held did exactly this in production on
//! 2026-08-25, and the orphaned worker it left behind is how it was
//! noticed. Two things came out of it. The bind moved out of this
//! module entirely — `run_daemon` takes the listener before it
//! registers a worker or publishes anything, so losing the address is
//! now an exit with no side effect to clean up, and the bound socket
//! doubles as the daemon's single-instance lock. And the worker row is
//! no longer this teardown's private responsibility: `WorkerRegistration`
//! is settled here on the paths this function owns and by `run_daemon`
//! on every path it does not, so no `?` can leave a row behind again.

use std::sync::Arc;

use anyhow::Context;
use fq_runtime::events::{
    Event, EventPayload, SystemShutdownPayload, SystemStartupPayload, SystemTaskFailedPayload,
};
use fq_runtime::llm::LlmClient;
use fq_runtime::worker::{DrainReason, DrainRequest};
use fq_runtime::{
    Config, ControlPlaneStore, EventBus, McpClientManager, PricingTable, ProjectionConsumer,
    ProjectionStore, SharedRegistry, TriggerDispatcher,
};
use uuid::Uuid;

use crate::OperatorDeps;
use crate::boot::runtime_db_paths;
use crate::control_commands::{DownMode, DownSignal, wait_for_down};
use crate::operator_surface::operator_registry;
use crate::resume::ResumeControl;
use crate::signals::{ShutdownSignals, describe_task_result};
use crate::worker_registration::WorkerRegistration;

mod teardown;

use teardown::DrainOutcome;

/// Everything the daemon worked out before it started anything.
///
/// One struct rather than twenty arguments: these are not independent
/// knobs, they are the assembled runtime, and the only caller builds
/// all of them in order.
pub(crate) struct Assembled {
    pub runtime_id: Uuid,
    pub version: &'static str,
    pub config: Config,
    pub bus: EventBus,
    pub db_paths: fq_runtime::RuntimeDbPaths,
    pub store: Arc<ProjectionStore>,
    pub cp_store: Arc<ControlPlaneStore>,
    pub worker_store: Arc<fq_runtime::WorkerStore>,
    pub registry: Arc<fq_runtime::AgentRegistry>,
    pub llm: Arc<dyn LlmClient>,
    pub pricing: Arc<PricingTable>,
    pub resume_runner: Arc<fq_runtime::ReducerRunner<fq_runtime::Harness>>,
    pub worker: Arc<dyn fq_runtime::Worker>,
    pub worker_id: fq_runtime::worker::WorkerId,
    /// The daemon's worker row, armed at registration. This teardown
    /// settles it; `run_daemon` settles whatever this one never reached.
    pub registration: WorkerRegistration,
    /// The socket `run_daemon` bound before anything else — the
    /// instance lock, now ready to be served.
    pub edge_listener: fq_edge::EdgeListener,
    /// The signal streams, installed at startup and held open so a
    /// second one during the drain means something (#509).
    pub signals: ShutdownSignals,
    pub mcp_manager: McpClientManager,
    pub agents_loaded: u32,
    pub pricing_entries: u32,
    pub resume_handles: Vec<tokio::task::JoinHandle<()>>,
}

/// Start the hosted tasks, wait for a stop, and shut them down.
pub(crate) async fn run_hosted(a: Assembled) -> anyhow::Result<()> {
    let Assembled {
        runtime_id,
        version,
        config,
        bus,
        db_paths,
        store,
        cp_store,
        worker_store,
        registry,
        llm,
        pricing,
        resume_runner,
        worker,
        worker_id,
        registration,
        edge_listener,
        mut signals,
        mut mcp_manager,
        agents_loaded,
        pricing_entries,
        resume_handles,
    } = a;
    // Publish a system.startup event before spawning any tasks.
    // If this fails the daemon cannot produce lifecycle events at
    // all, which is a bad starting point — bail out loudly.
    let startup_event = Event::system(
        runtime_id,
        EventPayload::SystemStartup(SystemStartupPayload {
            runtime_id,
            version: version.to_string(),
            nats_url: config.nats.url.clone(),
            agents_loaded,
            pricing_entries,
        }),
    );
    bus.publish(&startup_event)
        .await
        .context("failed to publish system.startup event")?;

    // Everything the edge's registry is built from, constructed
    // before it and before any task is spawned. These are plain
    // values — channels, an Arc, a watch — so hoisting them costs
    // nothing and buys the ordering the edge below depends on.
    let (watermark_tx, projection_watermark) = fq_runtime::watermark::channel();
    let (coord_watermark_tx, coordination_watermark) = fq_runtime::watermark::channel();
    let shared_registry: SharedRegistry = Arc::new(tokio::sync::RwLock::new(registry));
    let down_signal = DownSignal::new();
    let mut down_rx = down_signal.subscribe();
    let resume_control = Arc::new(ResumeControl {
        bus: bus.clone(),
        worker_store: worker_store.clone(),
        cp_store: cp_store.clone(),
        runner: resume_runner.clone(),
        registry: shared_registry.clone(),
        llm: llm.clone(),
    });

    // The authenticated operator edge (ADR-0006 + ADR-0031, plan
    // Phase 2): TLS + capability tokens over tarpc
    // `invoke`/`next_batch`. Identity (certificate + token root)
    // persists under the state dir; the first run mints it and writes
    // the admin token beside it (0600) — see `edge_identity`. Same
    // supervision posture as the read service: outside the supervised
    // set — an operator surface dying must not take the runtime down.
    // The socket is already bound; only the identity and the registry
    // are attached here.
    let (identity, _identity_dir) = crate::edge_identity::resolve(&config)?;
    // The operator surface: real declarations over the daemon's
    // read views, gated at the projection watermark (Phase 3).
    let edge_views = Arc::new(
        fq_runtime::views::Views::open(&db_paths) // allow-runtime-internals: allow-direct-store-open: daemon's own
            .await
            .context("edge: failed to open the read views")?,
    );
    let edge_registry = Arc::new(operator_registry(
        edge_views,
        fq_runtime::watermark::Horizon::new(vec![
            projection_watermark.clone(),
            coordination_watermark.clone(),
        ]),
        std::time::Duration::from_millis(config.edge.min_seq_wait_ms),
        OperatorDeps {
            bus: bus.clone(),
            projection: store.clone(),
            control_plane: cp_store.clone(),
            facts: daemon_facts(&config),
            // The same runner the dispatcher and startup recovery
            // drive invocations with — `invocation.drop` asks it
            // whether the target is live, and arms its halt.
            runner: resume_runner.clone(),
            resume: resume_control.clone(),
            // The same hot-swapped handle `fq reload` updates and
            // the dispatcher reads, so `fq agent list` answers
            // with the definitions this daemon would run.
            agents: shared_registry.clone(),
            machinery: crate::control_commands::MachineryDeps {
                agents: shared_registry.clone(),
                agents_dir: config.agents.directory.clone(),
                default_model: config.agents.default_model.clone(),
                // The same runner `fq down`'s drain suspends and the
                // teardown below waits on.
                worker: resume_runner.clone(),
                down: down_signal,
            },
        },
    )?);
    let (edge_addr, edge_serving) = edge_listener
        .serve(&identity, edge_registry, edge_limits(&config))
        .context("edge: failed to serve on the bound listener")?;

    // ---- Nothing above has spawned a task; nothing below may fail.
    // New fallible work goes above this line — see the module doc for
    // what a `?` down here costs. ----
    let (proj_shutdown_tx, proj_shutdown_rx) = tokio::sync::oneshot::channel();
    let projection_consumer =
        ProjectionConsumer::new(bus.clone(), store.clone()).with_watermark(watermark_tx);
    let mut projection_handle =
        tokio::spawn(async move { projection_consumer.run(proj_shutdown_rx).await });

    // Spawn the coordination consumer. Subscribes to
    // invocation lifecycle events and maintains the
    // coordination_invocation_owner / coordination_worker
    // state. Stale-worker sweep runs on a timer.
    let (coord_shutdown_tx, coord_shutdown_rx) = tokio::sync::oneshot::channel();
    let coord_consumer = fq_runtime::CoordinationConsumer::new(bus.clone(), cp_store.clone())
        .with_runtime_id(runtime_id)
        .with_worker_store(worker_store.clone())
        .with_self_worker_id(worker_id.as_str().to_string())
        // The coordination half of the read horizon (Phase 3c):
        // gated reads of the Invocation view wait on this mark too,
        // because the fold spans stores this consumer writes.
        .with_watermark(coord_watermark_tx);
    let mut coord_handle = tokio::spawn(async move { coord_consumer.run(coord_shutdown_rx).await });

    // Spawn the worker heartbeat consumer (control-plane side).
    // Receives `fq.worker.*.heartbeat` events and updates
    // `coordination_worker.last_heartbeat` so the stale-worker
    // sweep actually has fresh data to work with.
    let (hb_consumer_shutdown_tx, hb_consumer_shutdown_rx) = tokio::sync::oneshot::channel();
    let hb_consumer = fq_runtime::HeartbeatConsumer::new(bus.clone(), cp_store.clone());
    let mut hb_consumer_handle =
        tokio::spawn(async move { hb_consumer.run(hb_consumer_shutdown_rx).await });

    // Spawn the invocation summary consumer (#216) when `[summary]`
    // names a model. Reuses the daemon's LLM client (routing is
    // per-model) and pricing table; its spend is emitted under the
    // reserved `summary` agent id, never against an invocation.
    let (summary_shutdown_tx, summary_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let summary_handle = config.summary.model.clone().map(|model| {
        println!("  summariser:       {model}");
        let sc = fq_runtime::SummaryConsumer::new(
            bus.clone(),
            llm.clone(),
            pricing.clone(),
            model,
            config.summary.max_line_chars,
        );
        tokio::spawn(async move { sc.run(summary_shutdown_rx).await })
    });

    // Spawn the advisory watch (#169). Drains the captured JetStream
    // MAX_DELIVERIES advisories for the trigger stream and emits the
    // dead-letter events the dispatcher's inline path cannot: a crash
    // during the final delivery, and pre-bound poison triggers at
    // consumer-upgrade time.
    let (advisory_shutdown_tx, advisory_shutdown_rx) = tokio::sync::oneshot::channel();
    let advisory_watch = fq_runtime::AdvisoryWatch::new(bus.clone());
    let mut advisory_handle =
        tokio::spawn(async move { advisory_watch.run(advisory_shutdown_rx).await });

    // Spawn the worker heartbeat producer (worker side). Fires
    // a heartbeat immediately and then every 10s (the default
    // interval). Without this, the coordination consumer's
    // stale-worker sweep would mass-mark every worker stale at
    // 30s. In v2 this task moves into the dedicated Worker
    // process; in v1 it lives in the daemon alongside the other
    // managed tasks. It is deliberately among the LAST to be
    // stopped: a draining worker is still executing steps, and a
    // worker that stops heartbeating while it works is exactly what
    // the stale sweep is meant to report.
    let (hb_producer_shutdown_tx, hb_producer_shutdown_rx) = tokio::sync::oneshot::channel();
    let hb_producer =
        fq_runtime::worker::HeartbeatProducer::new(bus.clone(), worker_id.clone(), runtime_id);
    let mut hb_producer_handle =
        tokio::spawn(async move { hb_producer.run(hb_producer_shutdown_rx).await });

    // Spawn the archive-ack consumer (worker side). Listens on
    // `fq.worker.{worker_id}.invocation.archive_acked`; on
    // receipt deletes the matching local invocation_state row.
    // The companion retry sweeper (below) republishes
    // invocation.archived if an ack never arrives, so missed
    // acks are recovered without a durable consumer.
    let (archive_ack_shutdown_tx, archive_ack_shutdown_rx) = tokio::sync::oneshot::channel();
    let archive_ack_consumer =
        fq_runtime::ArchiveAckConsumer::new(bus.clone(), worker_id.clone(), worker_store.clone());
    let mut archive_ack_handle =
        tokio::spawn(async move { archive_ack_consumer.run(archive_ack_shutdown_rx).await });

    // Spawn the archive retry sweeper. Periodically lists
    // pending hand-offs and republishes invocation.archived
    // until the control-plane acks. Cadence + warn threshold
    // come from `[worker]` in fqd.toml.
    let (archive_retry_shutdown_tx, archive_retry_shutdown_rx) = tokio::sync::oneshot::channel();
    let archive_retry_sweeper =
        fq_runtime::ArchiveRetrySweeper::new(bus.clone(), worker_id.clone(), worker_store.clone())
            .with_retry_interval_ms(config.worker.archive_retry_interval_ms)
            .with_warn_after_ms(config.worker.archive_warn_after_ms);
    let mut archive_retry_handle =
        tokio::spawn(async move { archive_retry_sweeper.run(archive_retry_shutdown_rx).await });

    // Spawn the retention sweeps. Two windows, both from `[state]`.
    // `retention_days` bounds invocation_archive and projected `events`
    // rows (step 10), except cost-bearing rows, kept indefinitely so
    // spend figures survive retention. `stale_worker_retention_days`
    // bounds stale coordination_worker rows, which accrue one per
    // daemon restart and used to need `fq workers prune`. Either at  allow-dead-command: retired verb, named as history
    // < 0 skips its own sweep; both < 0 exits the task.
    let (retention_shutdown_tx, retention_shutdown_rx) = tokio::sync::oneshot::channel();
    let retention_sweeper = fq_runtime::control_plane::retention::RetentionSweeper::new(
        cp_store.clone(),
        &config.state,
    )
    .with_projection_store(store.clone());
    let mut retention_handle =
        tokio::spawn(async move { retention_sweeper.run(retention_shutdown_rx).await });

    // Build the swappable registry handle the dispatcher reads. The
    // dispatcher reads it per-trigger, so `fq reload` can hot-swap the
    // inner Arc for a freshly-loaded registry and have the *next*
    // trigger pick it up. In-flight invocations snapshot their config
    // at trigger time and are undisturbed by a swap (ADR-0020
    // refresh-between-invocations precedent).

    let drain_probe: Arc<dyn fq_runtime::Worker> = resume_runner.clone();

    // The stop switch `control.down` throws (`fq down`, issue #63). Two
    // listener tasks used to stand here — one per control subject, each
    // resubscribing on loss — and both are gone with the subjects: the
    // machinery verbs are commands on the edge, so the daemon answers
    // them on the transport it already serves rather than on a
    // best-effort core-NATS channel whose loss it had to survive. What
    // is left is the watch the select below waits on.

    // What `invocation.resume` (#373) runs against. It used to be a
    // NATS listener on a bespoke subject; the verb is a declared
    // command on the edge now, so this is built here and the registry
    // below holds it — there is no listener left to hand it to.

    // Spawn the trigger dispatcher. Its concurrency bound (#70) is
    // config, default 1 (serial) until the Phase-2 concurrency gate.
    let (disp_shutdown_tx, disp_shutdown_rx) = tokio::sync::oneshot::channel();
    let dispatcher = TriggerDispatcher::new(
        bus.clone(),
        shared_registry.clone(),
        worker,
        llm,
        config.worker.max_concurrent_invocations,
    );
    let mut dispatcher_handle = tokio::spawn(async move { dispatcher.run(disp_shutdown_rx).await });

    // The edge begins serving only once everything it reports on is
    // running. Binding happened in `run_daemon`, before this process
    // had touched any shared state at all, so a bind failure — a port
    // already held, most often by a daemon that has not finished
    // exiting — returned there with nothing to unwind.
    tokio::spawn(async move {
        edge_serving.await;
        tracing::warn!("edge exited; the operator edge is down until the daemon restarts");
    });

    println!();
    println!("Runtime ready. Press Ctrl-C to stop.");
    println!("  - projection consumer is materialising events into SQLite");
    println!("  - trigger dispatcher is listening on fq.trigger.*");
    println!("  - edge is listening on {edge_addr}");

    // Wait for either a shutdown signal (Ctrl-C / SIGTERM) or one of
    // the hosted tasks exiting prematurely. We watch the task handles
    // in the same select so a silent-failing task is caught
    // immediately instead of at shutdown time.
    let (mut shutdown_reason, clean_exit, failed_task): (
        &'static str,
        bool,
        Option<(&'static str, String)>,
    ) = tokio::select! {
        reason = signals.next() => {
            match reason {
                "ctrl_c" => {
                    println!();
                    println!("Received Ctrl-C, shutting down...");
                    ("ctrl_c", true, None)
                }
                "sigterm" => {
                    // SIGTERM means "terminate gracefully", so treat it as a
                    // graceful drain (ADR-0027): flip the shared drain signal
                    // — in-flight invocations suspend at their next step
                    // boundary, the dispatcher stops consuming — then run the
                    // bounded-wait teardown below, exactly like `fq down`.
                    // Ctrl-C stays a fast stop for interactive use. A second
                    // signal during that wait escalates to the immediate but
                    // still clean stop (#509) — the streams stay open for it.
                    println!();
                    println!("Received SIGTERM, draining...");
                    drain_probe
                        .request_drain(DrainRequest::new(DrainReason::Deploy))
                        .await;
                    ("sigterm", true, None)
                }
                // Listener could not be installed/received; the helper
                // already logged the cause. Treat as an unclean exit.
                other => (other, false, None),
            }
        }
        // The `control.down` command asked for an operator-initiated clean
        // stop (issue #63). `Now` skips the drain (SIGINT-equivalent clean
        // stop); `Drain` suspends to a step boundary first (the command
        // handler already flipped the drain signal). Both are clean exits,
        // so the teardown deregisters the worker either way.
        mode = wait_for_down(&mut down_rx) => {
            match mode {
                DownMode::Now => ("down_now", true, None),
                DownMode::Drain => ("down", true, None),
            }
        }
        result = &mut projection_handle => {
            let err_msg = describe_task_result("projection consumer", result);
            ("task_failed", false, Some(("projection_consumer", err_msg)))
        }
        result = &mut coord_handle => {
            let err_msg = describe_task_result("coordination consumer", result);
            ("task_failed", false, Some(("coordination_consumer", err_msg)))
        }
        result = &mut hb_consumer_handle => {
            let err_msg = describe_task_result("heartbeat consumer", result);
            ("task_failed", false, Some(("heartbeat_consumer", err_msg)))
        }
        result = &mut advisory_handle => {
            let err_msg = describe_task_result("advisory watch", result);
            ("task_failed", false, Some(("advisory_watch", err_msg)))
        }
        result = &mut hb_producer_handle => {
            let err_msg = describe_task_result("heartbeat producer", result);
            ("task_failed", false, Some(("heartbeat_producer", err_msg)))
        }
        result = &mut archive_ack_handle => {
            let err_msg = describe_task_result("archive-ack consumer", result);
            (
                "task_failed",
                false,
                Some(("archive_ack_consumer", err_msg)),
            )
        }
        result = &mut archive_retry_handle => {
            let err_msg = describe_task_result("archive retry sweeper", result);
            (
                "task_failed",
                false,
                Some(("archive_retry_sweeper", err_msg)),
            )
        }
        result = &mut retention_handle => {
            // RetentionSweeper::run returns () — a panic
            // shows up as Err(JoinError).
            match result {
                Ok(()) => (
                    "task_failed",
                    false,
                    Some(("retention_sweeper", "exited cleanly".to_string())),
                ),
                Err(err) => (
                    "task_failed",
                    false,
                    Some(("retention_sweeper", format!("task panicked: {err}"))),
                ),
            }
        }
        result = &mut dispatcher_handle => {
            // The dispatcher normally exits only on a fatal error. But a
            // graceful drain makes it stop consuming on its own once the
            // drain signal is set (PR-2), so if we're draining, its exit
            // is the clean drain path — not a task failure. (This also
            // covers the race where the dispatcher finishes draining
            // before the listener's `down_requested` signal is polled.)
            if drain_probe.drain_status() == fq_runtime::worker::DrainState::Draining {
                ("down", true, None)
            } else {
                let err_msg = describe_task_result("trigger dispatcher", result);
                ("task_failed", false, Some(("trigger_dispatcher", err_msg)))
            }
        }
    };
    // If a task failed, publish a system.task_failed event with
    // its details before we tear everything else down.
    if let Some((task_name, error_message)) = failed_task.as_ref() {
        tracing::error!(
            task = task_name,
            error = error_message.as_str(),
            "hosted task exited unexpectedly"
        );
        let failed_event = Event::system(
            runtime_id,
            EventPayload::SystemTaskFailed(SystemTaskFailedPayload {
                runtime_id,
                task_name: task_name.to_string(),
                error_message: error_message.clone(),
            }),
        );
        if let Err(err) = bus.publish(&failed_event).await {
            tracing::error!(error = %err, "failed to publish system.task_failed event");
        }
    }

    // The dispatcher stops consuming first, and nothing else is touched
    // until the drain has had its deadline. Every other task keeps
    // running through the wait — the heartbeat producer above all, so
    // the worker stays alive on the roster for as long as it is still
    // executing steps.
    let _ = disp_shutdown_tx.send(());

    // On a graceful drain (ADR-0027), wait — bounded by `drain_deadline_ms`
    // — for the invocation-bearing tasks (the dispatcher's in-flight run
    // and the recovery-resume tasks) to suspend at a step boundary. They
    // stop on their own because the drain signal is already set; past the
    // deadline the stragglers are hard-stopped and the next binary's
    // recovery resumes them. A second signal, or `fq down --now`, ends
    // the wait early — still cleanly (#509).
    // `fq down` (drain mode) and `fq down --now` both exit cleanly and
    // deregister the worker; only the drain-mode variants wait.
    let drained = matches!(shutdown_reason, "sigterm" | "down");
    if drained {
        println!();
        println!(
            "Draining — waiting up to {}ms for in-flight invocations to suspend...",
            config.drain_deadline_ms
        );
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(config.drain_deadline_ms);
        let outcome = teardown::wait_for_drain(
            deadline,
            dispatcher_handle,
            resume_handles,
            &mut signals,
            &mut down_rx,
        )
        .await;
        if let DrainOutcome::Escalated { reason } = outcome {
            shutdown_reason = match (shutdown_reason, reason) {
                ("sigterm", _) => "sigterm_escalated",
                _ => "down_escalated",
            };
        }
    } else {
        teardown::join_dispatcher(
            dispatcher_handle,
            tokio::time::Instant::now() + teardown::AUXILIARY_JOIN,
        )
        .await;
    }

    // The drain is over; everything else may stop now. Signalled
    // together and joined concurrently — these are consumers and
    // sweepers with nothing to suspend, so eight sequential five-second
    // joins only ever spent the operator's time (review B9).
    let _ = proj_shutdown_tx.send(());
    let _ = coord_shutdown_tx.send(());
    let _ = hb_consumer_shutdown_tx.send(());
    let _ = summary_shutdown_tx.send(());
    let _ = advisory_shutdown_tx.send(());
    let _ = hb_producer_shutdown_tx.send(());
    let _ = archive_ack_shutdown_tx.send(());
    let _ = archive_retry_shutdown_tx.send(());
    let _ = retention_shutdown_tx.send(());
    tokio::join!(
        teardown::join_fallible("projection consumer", projection_handle),
        teardown::join_fallible("coordination consumer", coord_handle),
        teardown::join_optional("summary consumer", summary_handle),
        teardown::join_fallible("heartbeat consumer", hb_consumer_handle),
        teardown::join_fallible("advisory watch", advisory_handle),
        teardown::join_fallible("heartbeat producer", hb_producer_handle),
        teardown::join_fallible("archive-ack consumer", archive_ack_handle),
        teardown::join_fallible("archive retry sweeper", archive_retry_handle),
        teardown::join_infallible("retention sweep", retention_handle),
    );

    // Shut down MCP server processes.
    mcp_manager.shutdown().await;

    // On a clean shutdown, deregister the worker so its coordination
    // row reflects a graceful exit (`shutdown`) instead of being left
    // `alive` to age into `stale` — the accumulation this fixes.
    // Symmetric with the startup `register_worker`; best-effort, a
    // failure here must never block the shutdown. A crash / task-
    // failure exit (`clean_exit == false`) is deliberately left to the
    // stale sweep, which is the honest signal that it did not exit
    // cleanly.
    registration.settle(clean_exit).await;

    // Publish a system.shutdown event on the way out. Best-effort —
    // if NATS is already unreachable we just log and continue. The
    // reason records the mode that actually ran, which is not always
    // the one that was asked for: a drain that was escalated says so.
    let shutdown_event = Event::system(
        runtime_id,
        EventPayload::SystemShutdown(SystemShutdownPayload {
            runtime_id,
            reason: shutdown_reason.to_string(),
            clean: clean_exit,
        }),
    );
    if let Err(err) = bus.publish(&shutdown_event).await {
        tracing::warn!(error = %err, "failed to publish system.shutdown event");
    }

    if failed_task.is_some() {
        anyhow::bail!("runtime exited because a hosted task failed");
    }
    Ok(())
}

/// What the edge will spend on connections it has not authenticated,
/// from `[edge]` in `fqd.toml`. Tunable parameters are configuration,
/// not code (Design Principle 8) — the defaults are in `EdgeConfig`.
fn edge_limits(config: &Config) -> fq_edge::EdgeLimits {
    fq_edge::EdgeLimits {
        max_connections: config.edge.max_connections,
        max_pre_auth_connections: config.edge.max_pre_auth_connections,
        max_concurrent_requests: config.edge.max_concurrent_requests,
        accept_error_backoff: std::time::Duration::from_millis(config.edge.accept_error_backoff_ms),
        ..fq_edge::EdgeLimits::default()
    }
}

/// What `control.status` answers about this daemon: where its state
/// lives and how long it will take to stop. All three come from the
/// config it was started with, so the report describes the process
/// reporting rather than the machine asking.
fn daemon_facts(config: &Config) -> crate::operator_surface::DaemonFacts {
    crate::operator_surface::DaemonFacts {
        db_paths: Arc::new(runtime_db_paths(config)),
        legacy_events_db: Arc::new(fq_runtime::db::legacy_db_path(&config.cache.directory)),
        drain_deadline_ms: config.drain_deadline_ms,
    }
}
