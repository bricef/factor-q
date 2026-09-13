//! The daemon-side maintenance consumer (#257).
//!
//! factor-q accumulates recurring housekeeping — a CAS reachability
//! audit, TTL sweeps, projection compaction, a pricing refresh — that
//! has to run on a cadence and has nothing to do with any agent's
//! work. **fq-cron is the scheduler**; this is the other half of that
//! answer. The adapter publishes a message to `fq.maintenance.<task>`
//! when a schedule fires, and the daemon runs the named task in
//! process and records how it went.
//!
//! Why the work runs here rather than in the adapter: a maintenance
//! task operates on the daemon's own state — its stores, its pricing
//! table, its projection — none of which an out-of-process scheduler
//! can reach. fq-cron therefore keeps knowing nothing about factor-q
//! beyond a subject and a payload (its "no daemon-side awareness"
//! non-goal is preserved in the direction that matters: the *adapter*
//! stays ignorant), and the daemon gains no scheduler.
//!
//! The three decisions worth reading before adding a task:
//!
//! - **The registry is a closed enum**, [`MaintenanceTask`]. A subject
//!   naming a task this build has no variant for is a typed refusal —
//!   [`UnknownTask`] — logged and published as a
//!   `Refused` [maintenance outcome](crate::events::MaintenanceOutcome)
//!   event, never a silently dropped
//!   message. A closed enum also means the compiler, not a reviewer,
//!   is what notices a new variant with no arm to run it.
//! - **Delivery is at-least-once and a run id is what makes it
//!   once.** #327 is unfixed and the ack window is finite, so a
//!   maintenance message can arrive twice. Every message carries a run
//!   id — fq-cron's `Nats-Msg-Id` (`fq-cron/<job>@<slot>`), or
//!   `seq:<stream sequence>` when it has none — and both spellings are
//!   stable across redeliveries of the same logical fire. The consumer
//!   keeps a bounded ledger of the run ids it has resolved and answers
//!   a repeat from the ledger instead of running the task again. The
//!   ledger is in-process and does not survive a restart, so it is a
//!   guard against redelivery, not a licence to register a task that
//!   would be wrong to run twice — see [`MaintenanceTask`].
//! - **A failed run is over.** The outcome event is published and the
//!   message acked; nothing is retried inside the ack loop, because
//!   the schedule *is* the retry and a redelivered maintenance run
//!   would stack up behind the next fire. The only NAK is a failure to
//!   publish the outcome, which redelivers a message the ledger then
//!   answers by re-publishing the recorded outcome — never by running
//!   the task a second time.
//!
//! Adding a task is a variant on [`MaintenanceTask`], an arm in its
//! `name`/`run` matches, and a line in the operating guide. Nothing in
//! this module's plumbing changes.

mod consumer;
mod task;

pub use consumer::{CONSUMER_NAME, MaintenanceConsumer, MaintenanceConsumerError};
pub use task::{MaintenanceContext, MaintenanceFailure, MaintenanceTask, UnknownTask};

#[cfg(test)]
mod tests;
