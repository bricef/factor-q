//! One durable consumer's cells in the health table.
//!
//! Its own module rather than another block in `render.rs`: that file
//! carries a size budget that may only ever go down, and a health table
//! that reports every consumer instead of one per stream is new code
//! (#549), not a line added to old code.

use fq_ops::health::{ConsumerHealth, ConsumerProgress};

use super::esc;

/// One durable's cells in the health table: name, state, pending, in
/// flight. A stuck consumer reads as stuck rather than as merely
/// lagging — the two look alike in a backlog column and are entirely
/// different problems (#549).
///
/// `pending` is the broker's `num_pending`, the matching messages
/// JetStream still owes this consumer, and `in flight` its
/// `ack_pending`, the ones already in its hands. Both are counted with
/// the consumer's subject filter applied, which the distance to the
/// stream head is not. The verdict itself is
/// [`ConsumerHealth::progress`], shared with `fq status`.
pub(super) fn consumer_row(consumer: &ConsumerHealth) -> (String, String, String, String) {
    match consumer {
        ConsumerHealth::Active {
            ack_pending,
            name,
            num_pending,
            num_redelivered,
            redeliveries,
            malformed_acked,
            ..
        } => {
            let state = match consumer.progress() {
                Some(ConsumerProgress::Stuck) => {
                    format!(r#"<span class="bad">✗ stuck ({redeliveries} redeliveries)</span>"#)
                }
                Some(ConsumerProgress::CaughtUp) => {
                    r#"<span class="ok">✓ caught up</span>"#.to_string()
                }
                Some(ConsumerProgress::SlightlyBehind) => {
                    r#"<span class="warn">◐ slightly behind</span>"#.to_string()
                }
                // An active consumer always has a verdict; `None` is
                // the states this arm is not.
                Some(ConsumerProgress::Lagging) | None => {
                    r#"<span class="bad">✗ lagging</span>"#.to_string()
                }
            };
            let redelivery_suffix = if *num_redelivered > 0 {
                format!(r#" / <span class="warn">redelivered {num_redelivered}</span>"#)
            } else {
                String::new()
            };
            // Poison acked and skipped is not a fault, but it is a
            // fact an operator should be able to see beside the
            // consumer that skipped it.
            let malformed_suffix = if *malformed_acked > 0 {
                format!(r#" / <span class="warn">malformed acked {malformed_acked}</span>"#)
            } else {
                String::new()
            };
            (
                esc(name),
                state,
                num_pending.to_string(),
                format!("{ack_pending}{redelivery_suffix}{malformed_suffix}"),
            )
        }
        // Halted on an event in a version this build does not read: the
        // message is unacked and the consumer holds there. A backlog
        // figure would only ever grow, so the cells say where it
        // stopped.
        ConsumerHealth::Halted {
            name, halted_on, ..
        } => (
            esc(name),
            format!(
                r#"<span class="bad">✗ halted on schema_version {} (reads {:?})</span>"#,
                halted_on.schema_version, halted_on.supported
            ),
            "-".to_string(),
            format!(
                "seq {} unacked",
                halted_on
                    .stream_seq
                    .map_or_else(|| "?".to_string(), |s| s.to_string())
            ),
        ),
        ConsumerHealth::Missing { name } => (
            esc(name),
            r#"<span class="muted">not present</span>"#.to_string(),
            "-".to_string(),
            "-".to_string(),
        ),
        ConsumerHealth::Error { name, error } => (
            esc(name),
            format!(r#"<span class="bad">✗ {}</span>"#, esc(error)),
            "-".to_string(),
            "-".to_string(),
        ),
    }
}
