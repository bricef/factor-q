//! The count on its own: what it admits, when each cap refuses, what a
//! new tool call resets, and which refusal is the first. The runner's
//! use of it — the reset at each tool call, the gate's place before the
//! evaluators — is driven through a live runner in `runner/tests.rs` and
//! through the sim's select loop in `test_support/sim.rs`.

use super::*;

fn limits(per_tool_call: u32, per_invocation: u32) -> ServerRequestLimits {
    ServerRequestLimits {
        per_tool_call,
        per_invocation,
    }
}

/// The decided defaults (#831).
#[test]
fn the_defaults_are_eight_per_tool_call_and_a_hundred_per_invocation() {
    assert_eq!(ServerRequestLimits::default(), limits(8, 100));
}

/// A tool call admits exactly its cap, then refuses, naming it — and a
/// refused request is not counted, so the next tool call's allowance is
/// whole again.
#[test]
fn a_tool_call_admits_its_cap_and_refuses_the_next() {
    let limits = limits(3, 100);
    let mut count = ServerRequestCount::default();
    count.begin_tool_call();
    for n in 1..=3 {
        assert_eq!(count.admit(limits), Ok(()), "request {n}");
    }
    let refused = count.admit(limits).expect_err("the fourth is over the cap");
    assert_eq!(refused.cap, Cap::PerToolCall);
    assert_eq!(refused.limit, 3);

    count.begin_tool_call();
    for n in 1..=3 {
        assert_eq!(count.admit(limits), Ok(()), "next call, request {n}");
    }
    assert!(count.admit(limits).is_err());
}

/// The invocation's count is not reset by a new tool call: four calls of
/// two requests each against a cap of five admit five.
#[test]
fn the_invocation_cap_spans_tool_calls() {
    let limits = limits(2, 5);
    let mut count = ServerRequestCount::default();
    let mut admitted = 0;
    let mut refused = Vec::new();
    for _ in 0..4 {
        count.begin_tool_call();
        for _ in 0..2 {
            match count.admit(limits) {
                Ok(()) => admitted += 1,
                Err(cap) => refused.push(cap.cap),
            }
        }
    }
    assert_eq!(admitted, 5);
    assert_eq!(refused, vec![Cap::PerInvocation; 3]);
}

/// When both caps are reached, the one no later tool call lifts is the
/// one named.
#[test]
fn the_invocation_cap_is_named_when_both_are_reached() {
    let limits = limits(2, 2);
    let mut count = ServerRequestCount::default();
    count.begin_tool_call();
    count.admit(limits).unwrap();
    count.admit(limits).unwrap();
    assert_eq!(count.admit(limits).unwrap_err().cap, Cap::PerInvocation);
}

/// Only the run's first refusal is marked first — that is the one the
/// runner logs at `warn` — whichever cap refuses later.
#[test]
fn only_the_first_refusal_of_a_run_is_marked_first() {
    let limits = limits(1, 3);
    let mut count = ServerRequestCount::default();
    let mut firsts = Vec::new();
    for _ in 0..4 {
        count.begin_tool_call();
        for _ in 0..2 {
            if let Err(refused) = count.admit(limits) {
                firsts.push((refused.cap, refused.first));
            }
        }
    }
    // Calls one and two each admit one and refuse one; call three's
    // first request is the invocation's third, and everything after it
    // meets the invocation cap.
    assert_eq!(
        firsts,
        vec![
            (Cap::PerToolCall, true),
            (Cap::PerToolCall, false),
            (Cap::PerInvocation, false),
            (Cap::PerInvocation, false),
            (Cap::PerInvocation, false),
        ]
    );
}

/// The message names the scope, the value and the setting.
#[test]
fn the_refusal_names_the_cap_and_its_setting() {
    let per_call = CapReached {
        cap: Cap::PerToolCall,
        limit: 8,
        first: true,
    };
    assert_eq!(
        per_call.to_string(),
        "this tool call has reached its cap of 8 server-initiated requests \
         (`[worker] max_server_requests_per_tool_call`)"
    );
    let per_invocation = CapReached {
        cap: Cap::PerInvocation,
        limit: 100,
        first: false,
    };
    assert_eq!(
        per_invocation.to_string(),
        "this invocation has reached its cap of 100 server-initiated requests \
         (`[worker] max_server_requests_per_invocation`)"
    );
}
