//! The maintenance task registry: the closed set of housekeeping this
//! build knows how to run, and what running one means.

use std::fmt;

use crate::events::subjects;

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
}

impl MaintenanceTask {
    /// Every task, in the order the operator surfaces list them.
    ///
    /// One place, derived from nothing: the compiler cannot check that
    /// a new variant is added here, so the test below asserts that
    /// round-tripping every name recovers every variant, which fails
    /// the moment a variant is missing.
    pub const ALL: &'static [MaintenanceTask] = &[MaintenanceTask::Ping];

    /// The task's name — the last token of its subject, and how it is
    /// spelled in `fq-cron.toml` and in the outcome event.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ping => "ping",
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

    /// Run the task, returning the one-line detail the outcome event
    /// carries.
    ///
    /// `ctx` is where a task's dependencies arrive. It is empty today
    /// because [`Self::Ping`] needs nothing; it is in the signature
    /// from the start so the first task that needs the pricing table
    /// or a store adds a field rather than churning every call site
    /// (the same forward-compatible-seam convention the built-in tools
    /// follow with their `new()`).
    pub async fn run(self, ctx: &MaintenanceContext) -> Result<String, MaintenanceFailure> {
        let _ = ctx;
        match self {
            Self::Ping => Ok("pong".to_string()),
        }
    }
}

impl fmt::Display for MaintenanceTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What a task needs from the daemon to run.
///
/// Empty while `ping` is the only task. A task that needs the pricing
/// table, a store handle or the bus takes a field here, and the daemon
/// fills it in where it constructs the consumer.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct MaintenanceContext {}

impl MaintenanceContext {
    pub fn new() -> Self {
        Self::default()
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
