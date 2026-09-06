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
