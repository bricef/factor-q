//! The P8 grammar, tested two ways: a property sweep over the name
//! space (any name in a class parses to that class's kind, invalid
//! shapes never parse), and the concrete op roster from the interface
//! inventory pinned name-by-name — so a vocabulary edit that would
//! reclassify a planned operation is a visible diff here, not a
//! surprise at migration time.

use fq_ops::OpKind;
use fq_ops::name::{COMMAND_VERBS, NameError, PROBE_NAMES, QUERY_NOUNS, expected_kind};
use proptest::prelude::*;

/// The planned steady-state runtime roster (interface inventory §1,
/// execution-plan Phase 4), each with the kind its migration slice
/// will declare. `agent.validate` is absent deliberately: the
/// inventory keeps it a local pure function, op exposure optional.
const ROSTER: &[(&str, OpKind)] = &[
    ("agent.list", OpKind::Query),
    ("agent.show", OpKind::Query),
    ("control.down", OpKind::Command),
    ("control.reload", OpKind::Command),
    ("cost.by_agent", OpKind::Query),
    ("cost.summary", OpKind::Query),
    ("deadletter.list", OpKind::Query),
    ("deadletter.requeue", OpKind::Command),
    ("event.query", OpKind::Query),
    ("event.tail", OpKind::Stream),
    ("invocation.drop", OpKind::Command),
    ("invocation.list", OpKind::Query),
    ("invocation.show", OpKind::Query),
    ("invocation.transcript", OpKind::Query),
    ("invocation.transcript.tail", OpKind::Stream),
    ("registry.describe", OpKind::Query),
    ("runtime.doctor", OpKind::Query),
    ("runtime.health", OpKind::Probe),
    ("runtime.status", OpKind::Probe),
    ("runtime.version", OpKind::Query),
    ("trigger.publish", OpKind::Command),
    ("traversal.run", OpKind::Command),
    ("traversal.status", OpKind::Query),
    ("traversal.tail", OpKind::Stream),
    ("worker.list", OpKind::Query),
    ("worker.prune", OpKind::Command),
    ("worker.show", OpKind::Query),
];

#[test]
fn the_planned_roster_parses_to_its_declared_kinds() {
    for (name, kind) in ROSTER {
        assert_eq!(
            expected_kind(name).as_ref(),
            Ok(kind),
            "`{name}` must parse as {kind:?}"
        );
    }
}

#[test]
fn probe_allowlist_beats_the_noun_vocabulary() {
    // `status` is a query noun (traversal.status), but the two probe
    // names stay probes — D2 keeps the kind deliberately small.
    assert_eq!(expected_kind("runtime.status"), Ok(OpKind::Probe));
    assert_eq!(expected_kind("traversal.status"), Ok(OpKind::Query));
}

/// A valid segment: lowercase alpha start, then lowercase/digit/underscore.
fn segment() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,11}"
}

proptest! {
    /// Any well-formed name ending `.tail` is a stream — resumability
    /// is a contract property carried by the name (D5, P8).
    #[test]
    fn tail_always_parses_as_stream(domain in segment(), mid in proptest::option::of(segment())) {
        let name = match mid {
            Some(mid) => format!("{domain}.{mid}.tail"),
            None => format!("{domain}.tail"),
        };
        prop_assert_eq!(expected_kind(&name), Ok(OpKind::Stream));
    }

    /// Any well-formed `<domain>.<command-verb>` is a command.
    #[test]
    fn command_verbs_parse_as_commands(domain in segment(), verb in proptest::sample::select(COMMAND_VERBS)) {
        let name = format!("{domain}.{verb}");
        prop_assert_eq!(expected_kind(&name), Ok(OpKind::Command));
    }

    /// Any well-formed `<domain>.<query-noun>` is a query — unless the
    /// full name is probe-allowlisted.
    #[test]
    fn query_nouns_parse_as_queries(domain in segment(), noun in proptest::sample::select(QUERY_NOUNS)) {
        let name = format!("{domain}.{noun}");
        prop_assume!(!PROBE_NAMES.contains(&name.as_str()));
        prop_assert_eq!(expected_kind(&name), Ok(OpKind::Query));
    }

    /// A leaf outside every vocabulary is refused, never guessed —
    /// extending the vocabulary is the deliberate curation gate (P11).
    #[test]
    fn unknown_leaves_are_refused(domain in segment(), leaf in segment()) {
        prop_assume!(leaf != "tail");
        prop_assume!(!COMMAND_VERBS.contains(&leaf.as_str()));
        prop_assume!(!QUERY_NOUNS.contains(&leaf.as_str()));
        let name = format!("{domain}.{leaf}");
        prop_assume!(!PROBE_NAMES.contains(&name.as_str()));
        prop_assert_eq!(
            expected_kind(&name),
            Err(NameError::UnknownLeaf { name: name.clone(), leaf })
        );
    }

    /// One segment is too few, four are too many.
    #[test]
    fn segment_count_is_two_or_three(a in segment(), b in segment(), c in segment(), d in segment()) {
        prop_assert!(matches!(expected_kind(&a), Err(NameError::SegmentCount(_))));
        let four = format!("{a}.{b}.{c}.{d}");
        prop_assert!(matches!(expected_kind(&four), Err(NameError::SegmentCount(_))));
    }

    /// Uppercase, hyphens, leading digits, empty segments: refused.
    #[test]
    fn malformed_segments_are_refused(domain in segment(), bad in "[A-Z0-9-][a-zA-Z0-9-]{0,5}") {
        let name = format!("{domain}.{bad}");
        let refused = matches!(expected_kind(&name), Err(NameError::BadSegment { .. }));
        prop_assert!(refused, "expected BadSegment for `{}`", name);
    }
}
