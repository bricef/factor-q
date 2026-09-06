use super::*;

/// The whole point of the escalation: the delay grows per redelivery
/// and then stops growing, so a handler failing forever costs one
/// round-trip per cap interval rather than thousands per second.
#[test]
fn nak_delay_doubles_then_caps() {
    let policy = ConsumerRedeliveryPolicy::default();
    assert_eq!(policy.nak_delay(1), Duration::from_secs(1));
    assert_eq!(policy.nak_delay(2), Duration::from_secs(2));
    assert_eq!(policy.nak_delay(3), Duration::from_secs(4));
    assert_eq!(policy.nak_delay(4), Duration::from_secs(8));
    assert_eq!(policy.nak_delay(5), Duration::from_secs(16));
    assert_eq!(policy.nak_delay(6), Duration::from_secs(32));
    // 64s would be next; the cap takes over and never lets go.
    assert_eq!(policy.nak_delay(7), Duration::from_secs(60));
    assert_eq!(policy.nak_delay(8), Duration::from_secs(60));
    assert_eq!(policy.nak_delay(1_000), Duration::from_secs(60));
}

/// A delivery count from a consumer that has been failing for weeks
/// must not overflow the doubling into a panic or a zero delay.
#[test]
fn nak_delay_saturates_rather_than_overflowing() {
    let policy = ConsumerRedeliveryPolicy::default();
    assert_eq!(policy.nak_delay(u64::MAX), policy.nak_max);
    // A delivery count of 0 should never reach us (JetStream counts
    // from 1), but it must still answer the first-delivery delay
    // rather than underflowing.
    assert_eq!(policy.nak_delay(0), policy.nak_initial);
}

/// A policy whose initial delay already exceeds the cap answers the
/// cap, not the initial — the cap is a ceiling, not a suggestion.
#[test]
fn nak_delay_never_exceeds_the_cap() {
    let policy = ConsumerRedeliveryPolicy {
        nak_initial: Duration::from_secs(120),
        nak_max: Duration::from_secs(60),
        ..ConsumerRedeliveryPolicy::default()
    };
    assert_eq!(policy.nak_delay(1), Duration::from_secs(60));
}

#[test]
fn escalation_steps_are_the_deliveries_whose_delay_changed() {
    let policy = ConsumerRedeliveryPolicy::default();
    for delivered in 1..=7 {
        assert!(
            policy.escalates_at(delivered),
            "delivery {delivered} changes the delay and is an escalation step"
        );
    }
    for delivered in 8..=20 {
        assert!(
            !policy.escalates_at(delivered),
            "delivery {delivered} is at the cap; the delay did not change"
        );
    }
}

/// The log rate B4 asks for: every escalation step says something new,
/// and once the delay caps the lines are one interval apart — not one
/// per redelivery, and never a line per broker round-trip.
#[test]
fn redelivery_log_admits_every_escalation_then_one_per_interval() {
    let policy = ConsumerRedeliveryPolicy::default();
    let mut log = RedeliveryLog::new(policy);
    let t0 = std::time::Instant::now();

    // Escalating: each step is admitted, at the instant JetStream
    // would actually redeliver.
    let mut at = t0;
    for delivered in 1..=7 {
        assert!(
            log.admit(delivered, at),
            "escalation step {delivered} must be logged"
        );
        at += policy.nak_delay(delivered);
    }
    // `at` has run past the last admitted line by one cap interval;
    // the suppression window is measured from the line itself.
    let last_line = at - policy.nak_delay(7);

    // Capped: a redelivery arriving before the interval has elapsed is
    // suppressed, and the one after it is not.
    assert!(!log.admit(8, last_line + Duration::from_secs(30)));
    assert!(!log.admit(9, last_line + Duration::from_secs(59)));
    assert!(log.admit(10, last_line + Duration::from_secs(60)));
    assert!(!log.admit(11, last_line + Duration::from_secs(61)));
}

/// The hot-loop case the rate limit exists for: a thousand
/// redeliveries inside one interval produce one line, not a thousand.
#[test]
fn redelivery_log_bounds_a_hot_loop_to_one_line() {
    let policy = ConsumerRedeliveryPolicy {
        // Everything at the cap from the first delivery, so no
        // redelivery is an escalation step and only the interval
        // governs.
        nak_initial: Duration::from_secs(60),
        ..ConsumerRedeliveryPolicy::default()
    };
    let mut log = RedeliveryLog::new(policy);
    let t0 = std::time::Instant::now();

    let mut admitted = 0;
    for i in 0..1_000u64 {
        // Every redelivery lands in the same second.
        if log.admit(2 + i, t0 + Duration::from_millis(i)) {
            admitted += 1;
        }
    }
    assert_eq!(
        admitted, 1,
        "a thousand redeliveries inside one interval is one line"
    );
}

/// The flood the per-loop limiter has to stop but a single-message test
/// cannot see: a handler failing on *every* message means delivery 1 of
/// a fresh message arrives over and over, and each one looks like a
/// first escalation step. Admitting them all would make the rate
/// `steps × arrival rate` — on a busy stream under a persistent
/// `SQLITE_FULL`, precisely the flood being prevented.
#[test]
fn a_second_message_does_not_climb_the_ladder_again() {
    let policy = ConsumerRedeliveryPolicy::default();
    let mut log = RedeliveryLog::new(policy);
    let t0 = std::time::Instant::now();

    // Message A climbs its ladder: seven lines.
    let mut at = t0;
    let mut admitted = 0;
    for delivered in 1..=7 {
        if log.admit(delivered, at) {
            admitted += 1;
        }
        at += policy.nak_delay(delivered);
    }
    assert_eq!(admitted, 7, "the first fault shows its whole escalation");
    // The window is measured from the last line, not from the end of
    // the ladder — the seventh step's own delay has not elapsed yet.
    let last_line = at - policy.nak_delay(7);

    // Message B, interleaved inside the same window, starts at
    // delivery 1 again. Not one of its steps is new information.
    let mut b_admitted = 0;
    for delivered in 1..=7 {
        if log.admit(delivered, last_line + Duration::from_millis(delivered)) {
            b_admitted += 1;
        }
    }
    assert_eq!(
        b_admitted, 0,
        "a second failing message inside the window says nothing new"
    );

    // A hundred more messages, same window, same silence.
    let mut flood = 0;
    for i in 0..100u64 {
        for delivered in 1..=7 {
            if log.admit(delivered, last_line + Duration::from_millis(10 + i)) {
                flood += 1;
            }
        }
    }
    assert_eq!(flood, 0, "the flood is the case; it must cost nothing");
}

/// The window reopening is not a licence to flood either: one line, and
/// then the ladder may be climbed again only as far as it actually goes.
#[test]
fn the_window_reopening_admits_one_line_then_resumes_the_ladder() {
    let policy = ConsumerRedeliveryPolicy::default();
    let mut log = RedeliveryLog::new(policy);
    let t0 = std::time::Instant::now();

    assert!(log.admit(1, t0), "the first failure speaks");
    assert!(!log.admit(1, t0 + Duration::from_secs(1)), "same step");

    // A minute later the window reopens: one line.
    let reopened = t0 + policy.log_interval;
    assert!(log.admit(1, reopened));
    // And immediately after it, a fresh message's delivery 1 is not a
    // second line.
    assert!(!log.admit(1, reopened + Duration::from_millis(1)));
    // A genuinely higher step inside the new window is.
    assert!(log.admit(2, reopened + Duration::from_millis(2)));
}
