//! Startup recovery decides only after the coordination backlog is folded
//! (issue 1070).
//!
//! A daemon can stop with events on the stream that its coordination
//! consumer never folded: an operator's drop, or a run's own `failed`,
//! published just before it died. Recovery reads the ownership rows and
//! the WAL, and those are what the fold writes, so a recovery that ran
//! first would find such a run in flight and resume it, under a drop or
//! after its ending. So the boot reads the stream's last sequence first
//! (the boot point), starts the coordination consumer, and waits, bounded
//! by `[worker] recovery_backlog_wait_secs`, until the consumer has folded
//! that far. Only then does recovery reconcile, classify, restore and
//! resume, as it always has. Normally the wait is a moment: the backlog is
//! what the dead daemon had not yet folded.
//!
//! When the bound is hit, the boot goes on and recovery decides as before,
//! except that it resumes none of the runs it would have: it cannot tell
//! which of them the unfolded events end. A consumer that halts on an event
//! this build does not read ends the wait at once: it folds nothing more in
//! this process however long the boot waits, so the boot goes on the same
//! way without spending the bound. The held runs are logged at error and
//! stay in flight until a later start finds the events folded, which is
//! the way out: a restart once the consumer has caught up. Nothing in this
//! process resumes them (issue 1089), and no operator command does:
//! `fq invocation resume` takes only ambiguous runs. A second start while
//! the fold is still behind holds them again.
//!
//! The consumer's tick sweeps are held until recovery has decided, so
//! that neither reports the dead daemon's worker or its runs before it has
//! (`hold_sweeps`). They are released then whether or not it held any run
//! back: a held run is reported stuck once past its threshold, as any run
//! that has not moved is.
//!
//! What a fold does this early in the boot needs nothing that is not
//! running yet, so nothing is lost and nothing waits on a later part of
//! the boot:
//!
//! - It writes the control-plane store and, for a drop, the WAL; both are
//!   open before recovery starts.
//! - It answers an `invocation.archived` with `invocation.archive_acked`
//!   on the bus. The event stream stores it whether or not anyone
//!   subscribes, so the publish neither fails nor waits. Nobody hears an
//!   acknowledgement for an archive in the backlog: it names the dead
//!   daemon's worker, whose subject nobody subscribes to again, as was so
//!   before this change. The new daemon's archive sweeper republishes
//!   every terminal WAL row it holds once `run_hosted` starts it, and that
//!   acknowledgement reaches the archive-ack consumer started beside it.
//! - Its logs go to the subscriber installed before the boot, and what it
//!   counts for `fq doctor` (a halt, malformed events acknowledged) to the
//!   ledger on the bus handle, made when the bus connected.
//! - Its tick sweeps are held (above).
//!
//! The consumer's task is supervised from `run_hosted`; if it ends during
//! the wait, the wait stops early and `run_hosted` reports the exit.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use fq_runtime::bus::STREAM_NAME;
use fq_runtime::control_plane::coordination_consumer::{CONSUMER_NAME, SweepRelease, hold_sweeps};
use fq_runtime::watermark::Watermark;
use fq_runtime::worker::{ClassifiedInvocation, WorkerId};
use fq_runtime::{Config, ControlPlaneStore, CoordinationConsumerError, EventBus, WorkerStore};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// How often the wait looks at the consumer's progress.
const POLL: Duration = Duration::from_millis(50);

/// The coordination consumer the boot started, for `run_hosted` to
/// supervise and stop with the rest.
pub(crate) struct Coordination {
    pub handle: JoinHandle<Result<(), CoordinationConsumerError>>,
    pub shutdown: oneshot::Sender<()>,
    /// The coordination half of the edge's read horizon.
    pub watermark: Watermark,
}

/// Whether the consumer has folded to the boot point.
#[derive(Clone)]
pub(crate) struct FoldProbe {
    pub bus: EventBus,
    /// The durable whose acknowledgements count as folded.
    pub durable: String,
    pub watermark: Watermark,
    /// The event stream's last sequence when the boot began.
    pub boot_point: u64,
}

impl FoldProbe {
    /// How far the consumer has folded: its mark, or its durable's ack
    /// floor where that is further. The mark moves only with an event this
    /// process folded, so a durable that had folded everything before the
    /// last daemon stopped is read from the broker; an unreadable durable
    /// counts as not there yet.
    pub async fn folded_to(&self) -> u64 {
        let mark = self.watermark.current();
        if mark >= self.boot_point {
            return mark;
        }
        let floor = match self.bus.jetstream().get_stream(STREAM_NAME).await {
            Ok(stream) => match stream.consumer_info(&self.durable).await {
                Ok(info) => info.ack_floor.stream_sequence,
                Err(_) => 0,
            },
            Err(_) => 0,
        };
        mark.max(floor)
    }
}

/// What the wait found, and the release of the consumer's tick sweeps.
pub(crate) struct Backlog {
    probe: FoldProbe,
    /// `None`: folded to the boot point. `Some`: how far it got.
    behind: Option<u64>,
    /// The wait ended because the consumer halted on an event this build
    /// does not read.
    halted: bool,
    sweeps: SweepRelease,
}

/// Read the boot point, start the coordination consumer, and wait for it
/// to fold that far, for at most `[worker] recovery_backlog_wait_secs`.
///
/// Called before startup recovery publishes anything, so that the boot
/// point is the end of what the stopped daemon left. The consumer is built
/// as `run_hosted` built it before this change, with its tick sweeps held.
pub(crate) async fn fold_backlog(
    bus: &EventBus,
    cp_store: &Arc<ControlPlaneStore>,
    worker_store: &Arc<WorkerStore>,
    runtime_id: Uuid,
    worker_id: &WorkerId,
    config: &Config,
) -> anyhow::Result<(Coordination, Backlog)> {
    let boot_point = bus
        .jetstream()
        .get_stream(STREAM_NAME)
        .await
        .context("startup recovery: failed to read the event stream")?
        .get_info()
        .await
        .context("startup recovery: failed to read the event stream's last sequence")?
        .state
        .last_sequence;

    let (mark, watermark) = fq_runtime::watermark::channel();
    let (hold, sweeps) = hold_sweeps();
    let (shutdown, stopped) = oneshot::channel();
    let consumer = fq_runtime::CoordinationConsumer::new(bus.clone(), cp_store.clone())
        .with_runtime_id(runtime_id)
        .with_worker_store(worker_store.clone())
        .with_self_worker_id(worker_id.as_str().to_string())
        // The stuck-invocation sweep (#37) rides the same tick as the
        // stale-worker one, at the threshold derived from this daemon's
        // call deadlines — the same number `fq doctor` and
        // `control.status` report.
        .with_stuck_sweep(Arc::new(
            fq_runtime::control_plane::liveness::StuckSweep::new(
                bus.clone(),
                worker_store.clone(),
                cp_store.clone(),
                config.stuck_after_ms(),
            ),
        ))
        // The coordination half of the read horizon (Phase 3c): gated
        // reads of the Invocation view wait on this mark too, because the
        // fold spans stores this consumer writes. It is also what this
        // wait reads.
        .with_watermark(mark)
        .with_sweeps_held(hold);
    let handle = tokio::spawn(async move { consumer.run(stopped).await });

    let probe = FoldProbe {
        bus: bus.clone(),
        durable: CONSUMER_NAME.to_string(),
        watermark: watermark.clone(),
        boot_point,
    };
    let bound = config.worker.recovery_backlog_wait();
    let started = tokio::time::Instant::now();
    let mut halted = false;
    let behind = loop {
        let folded_to = probe.folded_to().await;
        if folded_to >= boot_point {
            tracing::info!(
                boot_point,
                folded_to,
                waited_ms = started.elapsed().as_millis() as u64,
                "the coordination consumer has folded the events on the stream; startup \
                 recovery decides"
            );
            break None;
        }
        // A consumer halted on an event this build does not read folds
        // nothing more in this process, however long the boot waits. It has
        // said why at error, and `fq doctor` reports it from the ledger.
        if let Some(on) = bus.consumer_ledger().record(CONSUMER_NAME).halted_on {
            tracing::warn!(
                boot_point,
                folded_to,
                halted_at = on.stream_seq.unwrap_or(0),
                schema_version = on.schema_version,
                "the coordination consumer halted on an event this build does not read; \
                 startup recovery stops waiting for it"
            );
            halted = true;
            break Some(folded_to);
        }
        // A consumer that stopped folds nothing more; `run_hosted` reports
        // its exit as a task failure.
        if handle.is_finished() || started.elapsed() >= bound {
            break Some(folded_to);
        }
        tokio::time::sleep(POLL).await;
    };
    let coordination = Coordination {
        handle,
        shutdown,
        watermark,
    };
    let backlog = Backlog {
        probe,
        behind,
        halted,
        sweeps,
    };
    Ok((coordination, backlog))
}

impl Backlog {
    /// Split the runs recovery would resume into those it resumes now and
    /// those it holds: all of them when the consumer is behind, each
    /// logged at error, and none otherwise. Recovery has decided by then,
    /// so the release of the consumer's tick sweeps comes back with them,
    /// for `run_hosted` to release where it used to start the consumer.
    pub(crate) fn hold(
        self,
        recoverable: Vec<ClassifiedInvocation>,
    ) -> (Vec<ClassifiedInvocation>, SweepRelease) {
        let Backlog {
            probe,
            behind,
            halted,
            sweeps,
        } = self;
        let Some(folded_to) = behind else {
            return (recoverable, sweeps);
        };
        let why = if halted {
            "the coordination consumer halted on an event this build does not read before it \
             had folded the events on the stream"
        } else {
            "the coordination consumer had not folded the events on the stream within \
             [worker] recovery_backlog_wait_secs"
        };
        if recoverable.is_empty() {
            tracing::warn!(
                boot_point = probe.boot_point,
                folded_to,
                "{why}; startup recovery had nothing to resume"
            );
        }
        for run in &recoverable {
            tracing::error!(
                invocation_id = %run.state.invocation_id,
                agent_id = %run.state.agent_id,
                boot_point = probe.boot_point,
                folded_to,
                "not resuming this invocation at startup: {why}, and one of them may end it. \
                 It stays in flight until a later start finds them folded: restart the daemon \
                 once the consumer has caught up. If `fq doctor` reports the coordination \
                 consumer halted, no restart helps until a build that reads the event it halted \
                 on runs"
            );
        }
        (Vec::new(), sweeps)
    }
}
