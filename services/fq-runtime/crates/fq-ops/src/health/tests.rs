//! The consumer verdict, which is the one piece of judgement this
//! crate makes rather than merely declares.

use super::*;

fn active(stuck: bool, ack_pending: u64, num_pending: u64, delivered: u64) -> ConsumerHealth {
    ConsumerHealth::Active {
        name: "fq-summary".to_string(),
        delivered,
        ack_pending,
        num_pending,
        num_redelivered: 0,
        redeliveries: 0,
        stuck,
        malformed_acked: 0,
    }
}

/// The live reading from the dogfood broker that made this a bug: a
/// filtered summariser sitting 781 sequences below the stream head,
/// with nothing pending and nothing in flight, on a stream whose newest
/// matching message is exactly where it stands. Every message above it
/// is heartbeat and coordination traffic it does not subscribe to and
/// will never be offered.
///
/// It read `✗ lagging` for as long as the field behind that verdict was
/// the distance to the head. The broker's own figures say caught up.
#[test]
fn a_filtered_consumer_with_nothing_pending_is_caught_up_wherever_it_sits() {
    assert_eq!(
        active(false, 0, 0, 460_496).progress(),
        Some(ConsumerProgress::CaughtUp)
    );
}

#[test]
fn the_verdict_reads_the_backlog_and_the_work_in_hand() {
    assert_eq!(
        active(false, 0, 0, 10).progress(),
        Some(ConsumerProgress::CaughtUp)
    );
    // Delivered and unacked is work in hand, so it is not caught up
    // even with an empty backlog.
    assert_eq!(
        active(false, 1, 0, 10).progress(),
        Some(ConsumerProgress::SlightlyBehind)
    );
    assert_eq!(
        active(false, 0, 3, 10).progress(),
        Some(ConsumerProgress::SlightlyBehind)
    );
    assert_eq!(
        active(false, 0, SLIGHTLY_BEHIND_BELOW - 1, 10).progress(),
        Some(ConsumerProgress::SlightlyBehind)
    );
    assert_eq!(
        active(false, 0, SLIGHTLY_BEHIND_BELOW, 10).progress(),
        Some(ConsumerProgress::Lagging)
    );
    assert_eq!(
        active(false, 0, 99, 10).progress(),
        Some(ConsumerProgress::Lagging)
    );
}

/// Stuck outranks the backlog: a consumer retrying one message forever
/// can have nothing else pending, and "caught up" is the last thing an
/// operator should read off it.
#[test]
fn stuck_outranks_an_empty_backlog() {
    assert_eq!(
        active(true, 1, 0, 10).progress(),
        Some(ConsumerProgress::Stuck)
    );
    assert!(active(true, 1, 0, 10).is_fault());
}

/// The states where "how far behind" is not the finding have no
/// verdict at all, so a surface cannot render one by accident.
#[test]
fn a_consumer_that_is_not_working_has_no_progress_verdict() {
    assert_eq!(
        ConsumerHealth::Missing {
            name: "fq-summary".to_string()
        }
        .progress(),
        None
    );
    assert_eq!(
        ConsumerHealth::Error {
            name: "fq-summary".to_string(),
            error: "timed out".to_string(),
        }
        .progress(),
        None
    );
    assert_eq!(
        ConsumerHealth::Halted {
            name: "fq-summary".to_string(),
            halted_on: UnsupportedEvent {
                schema_version: 2,
                supported: vec![3],
                event_id: None,
                subject: "fq.agent.researcher.completed".to_string(),
                stream_seq: Some(42),
            },
            malformed_acked: 0,
        }
        .progress(),
        None
    );
}

/// A backlog is not a fault, however large: a consumer catching up
/// after a restart is working, and the verdict is a reading for a
/// human rather than an exit code.
#[test]
fn a_large_backlog_is_a_verdict_and_not_a_fault() {
    let behind = active(false, 1, 9_000, 10);
    assert_eq!(behind.progress(), Some(ConsumerProgress::Lagging));
    assert!(!behind.is_fault());
}
