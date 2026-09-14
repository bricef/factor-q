//! The maintenance task registry: the closed set of housekeeping this
//! build knows how to run, and what running one means.

use std::fmt;

use crate::events::{OperatorSignalPayload, subjects};
use crate::pricing::refresh::PricingRefresh;

/// Every maintenance task this build can run, as values.
///
/// **Closed, and a `&str` is never one of them.** The consumer reads a
/// task *name* off a NATS subject, which is untrusted input from
/// outside the process; [`Self::parse`] is the only way in, and its
/// error is a value the consumer publishes rather than a message it
/// drops. A string-keyed registry would have made an unknown name look
/// exactly like a name whose handler had not been registered yet.
///
/// **Membership carries an obligation: a task must be safe to run
/// twice.** The consumer's run-id ledger suppresses a redelivery
/// within one daemon's lifetime, but it is in-process — a restart
/// between a run and its ack starts the ledger empty, and the
/// redelivered message runs the task again. So every variant here is
/// convergent by construction (a refresh that overwrites, an audit
/// that recomputes, a sweep that deletes what is already past its
/// window), never accumulative. A task that must run exactly once
/// belongs behind exactly-once dispatch
/// (<https://github.com/bricef/factor-q/issues/327>), not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MaintenanceTask {
    /// The plumbing task: does nothing, succeeds, and says so.
    ///
    /// It exists to make the whole path — a schedule in
    /// `fq-cron.toml`, a publish to `fq.maintenance.ping`, the
    /// durable, the registry lookup, the outcome event — provable end
    /// to end without depending on whatever the first real task turns
    /// out to do. Keep it: it is what an operator reaches for to
    /// answer "is maintenance wired up on this instance?", and it is
    /// the one task whose failure can only mean the plumbing.
    Ping,

    /// Fetch the live pricing document, put it through acceptance, and
    /// swap the result into the table this daemon is serving
    /// (<https://github.com/bricef/factor-q/issues/344>).
    ///
    /// Convergent, as membership here requires: it overwrites a table
    /// with whatever the source currently says, so running it twice
    /// leaves exactly what running it once did. It is also the reason
    /// `[maintenance] ack_wait_ms` defaults to a minute rather than a
    /// second — this one reaches the network.
    PricingRefresh,
}

impl MaintenanceTask {
    /// Every task, in the order the operator surfaces list them.
    ///
    /// One place, derived from nothing: the compiler cannot check that
    /// a new variant is added here, so the test below asserts that
    /// round-tripping every name recovers every variant, which fails
    /// the moment a variant is missing.
    pub const ALL: &'static [MaintenanceTask] =
        &[MaintenanceTask::Ping, MaintenanceTask::PricingRefresh];

    /// The task's name — the last token of its subject, and how it is
    /// spelled in `fq-cron.toml` and in the outcome event.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::PricingRefresh => "pricing_refresh",
        }
    }

    /// The subject a scheduler publishes to in order to run this task.
    pub fn subject(self) -> String {
        subjects::maintenance(self.name())
    }

    /// Resolve a name off the wire, or refuse it by name.
    pub fn parse(token: &str) -> Result<Self, UnknownTask> {
        Self::ALL
            .iter()
            .copied()
            .find(|task| task.name() == token)
            .ok_or_else(|| UnknownTask {
                task: token.to_string(),
                known: Self::ALL
                    .iter()
                    .map(|task| task.name())
                    .collect::<Vec<_>>()
                    .join(", "),
            })
    }

    /// Run the task, returning what the outcome event should say and
    /// anything an operator should be told separately.
    ///
    /// `ctx` is where a task's dependencies arrive — the seam
    /// [`Self::PricingRefresh`] was the first to use, which is why the
    /// argument was in the signature before there was anything to put in
    /// it.
    pub async fn run(self, ctx: &MaintenanceContext) -> Result<TaskOutcome, MaintenanceFailure> {
        match self {
            Self::Ping => Ok(TaskOutcome::new("pong")),
            Self::PricingRefresh => {
                let outcome = ctx
                    .pricing()?
                    .run()
                    .await
                    .map_err(MaintenanceFailure::new)?;
                Ok(TaskOutcome::new(outcome.report.detail()).with_signals(outcome.signals))
            }
        }
    }
}

/// What a task run produced: the line the outcome event carries, and
/// whatever it wants an operator told.
///
/// The signals are returned rather than published, so a task never holds
/// the bus. The consumer publishes them beside the outcome it already
/// publishes, which is also what makes "once per run, never on a
/// redelivery" one rule in one place rather than a rule per task.
#[derive(Debug, Default)]
pub struct TaskOutcome {
    /// The one-line detail on the `maintenance_run` event.
    pub detail: String,
    /// Operator signals to publish with the outcome, in order.
    pub signals: Vec<OperatorSignalPayload>,
}

impl TaskOutcome {
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            signals: Vec::new(),
        }
    }

    pub fn with_signals(mut self, signals: Vec<OperatorSignalPayload>) -> Self {
        self.signals = signals;
        self
    }
}

impl fmt::Display for MaintenanceTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What a task needs from the daemon to run.
///
/// Every field is optional and every task that needs one says so with a
/// typed failure rather than a panic. The reason is that a consumer is
/// constructible without a daemon around it — the tests do it, and so
/// would any future embedding — and a task asked to run with nothing to
/// run against should leave a record saying exactly that, not take the
/// process down.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct MaintenanceContext {
    pricing: Option<PricingRefresh>,
}

impl MaintenanceContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wire the pricing refresh: the settings, the cache and the served
    /// table the daemon booted with.
    pub fn with_pricing(mut self, refresh: PricingRefresh) -> Self {
        self.pricing = Some(refresh);
        self
    }

    /// The refresh, or the failure a run records when this daemon wired
    /// none.
    fn pricing(&self) -> Result<&PricingRefresh, MaintenanceFailure> {
        self.pricing.as_ref().ok_or_else(|| {
            MaintenanceFailure::new(
                "this daemon has no pricing refresh wired, so there is no table to refresh",
            )
        })
    }
}

/// A subject named a task this build has no registry entry for.
///
/// A value rather than a log line: it is published as the `Refused`
/// variant of [`crate::events::MaintenanceOutcome`], so a scheduler
/// pointed at a task that a rollback removed — or misspelled in
/// `fq-cron.toml` — leaves a record an operator can find, instead of
/// commands that vanish on arrival.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("no maintenance task named {task:?} in this build (known: {known})")]
pub struct UnknownTask {
    /// The token exactly as it appeared on the subject.
    pub task: String,
    /// The task names this build does know, comma-separated.
    pub known: String,
}

/// A task ran and failed. Wraps whatever the task's own error was.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct MaintenanceFailure(Box<dyn std::error::Error + Send + Sync>);

impl MaintenanceFailure {
    pub fn new(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(err.into())
    }
}
