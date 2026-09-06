use super::*;

fn token(n: i64) -> ProgressToken {
    ProgressToken(NumberOrString::Number(n))
}

fn issue(registry: &ProgressRegistry, server: &str, n: i64, call_id: &str) -> InFlightGuard {
    registry.issue(
        server,
        &token(n),
        format!("inv-for-{call_id}"),
        call_id.to_string(),
        format!("{server}__search"),
    )
}

/// The whole point of #605: the token rmcp minted routes back to the
/// invocation and call that issued the request, and the entry goes
/// away with the call's scope.
#[test]
fn progress_is_attributed_to_the_issuing_call_and_cleared_after() {
    let registry = ProgressRegistry::default();
    {
        let _guard = issue(&registry, "docs", 0, "call-a");
        let attributed = registry
            .record_progress("docs", "0", 3.0, Some(10.0))
            .expect("a registered token attributes");
        assert_eq!(attributed.invocation_id, "inv-for-call-a");
        assert_eq!(attributed.call_id, "call-a");
        assert!(attributed.last_progress_at.is_some());
    }

    assert!(registry.is_empty(), "the guard clears the entry");
    assert!(
        registry.record_progress("docs", "0", 4.0, None).is_none(),
        "progress against a finished call attributes to nothing"
    );
}

/// rmcp numbers tokens from zero per peer, so two servers both issue a
/// token `0`. The key has to carry the server or one server's progress
/// lands on the other's call.
#[test]
fn identical_tokens_from_two_servers_do_not_collide() {
    let registry = ProgressRegistry::default();
    let docs = issue(&registry, "docs", 0, "call-a");
    let _shell = issue(&registry, "shell", 0, "call-b");

    assert_eq!(
        registry
            .record_progress("docs", "0", 1.0, None)
            .unwrap()
            .call_id,
        "call-a"
    );
    assert_eq!(
        registry
            .record_progress("shell", "0", 1.0, None)
            .unwrap()
            .call_id,
        "call-b"
    );

    // And one finishing leaves the other alone.
    drop(docs);
    assert_eq!(registry.in_flight().len(), 1);
    assert_eq!(registry.in_flight()[0].call_id, "call-b");
}

/// The rate limit is on the log line, never on the fact: a stuck-call
/// detector reads `last_progress_at`, so every notification must stamp
/// it however few of them are logged.
#[test]
fn every_notification_stamps_liveness_even_when_the_log_is_rate_limited() {
    let registry = ProgressRegistry::default();
    let _guard = issue(&registry, "docs", 7, "call-a");
    let mut stamps = Vec::new();
    for step in 1..=5 {
        stamps.push(
            registry
                .record_progress("docs", "7", f64::from(step), None)
                .unwrap()
                .last_progress_at
                .expect("stamped"),
        );
    }
    assert!(
        stamps.windows(2).all(|pair| pair[1] >= pair[0]),
        "liveness advances on every notification"
    );
    assert_eq!(registry.in_flight().len(), 1);
    assert!(registry.in_flight()[0].silent_for() < Duration::from_secs(1));
}
