//! The published surface carries no issue numbers.
//!
//! A declaration's `summary` and `description`, and the doc comments
//! schemars lifts off every field, are not internal notes. They are the
//! contract: `describe` serves them, `fq ops list` prints them, and the
//! MCP face (Phase 6) hands them to a model choosing a tool. The reader
//! has none of this repository's context and no access to its issue
//! tracker, so `#130` tells them nothing and costs them a detour.
//!
//! That is a rule prose cannot keep. Thirteen references accumulated
//! before anyone noticed, several written by people who knew better and
//! were reviewing for accuracy rather than for whether a stranger could
//! use the sentence. So it is a gate, and an exception is a diff.
//!
//! **This reads the committed snapshot rather than building the
//! registry.** That is not a shortcut: `operator_surface.rs` asserts the
//! snapshot equals the live surface, so checking the snapshot checks the
//! surface transitively — and it does so without a broker, which keeps
//! this gate cheap enough to never be the reason someone skips a run.
//!
//! What it cannot check is whether a description is any *good*. "A
//! reader without your context can act on this" is not a testable
//! property. The gate stops one specific regression; the judgement
//! belongs at the `description()` builders, which say who reads them.

use std::path::Path;

/// Issue references the surface may carry, each with the reason it
/// earns its place.
///
/// Keep this list short. The bar is a live caveat about the numbers a
/// consumer is looking at, in `see #N` form, where pointing at the
/// tracking issue is the useful thing to do — not a provenance note
/// about why a behaviour exists, which is what a doc comment or a
/// commit message is for.
const SANCTIONED: &[(&str, &str)] = &[(
    "#50",
    "`DoctorExecutions` reads the worker-local `invocation_state` table \
     because the control plane's `in_flight` is not populated by trigger \
     dispatch yet. A consumer comparing doctor's numbers against the \
     control plane needs to know why they differ, and where that is \
     tracked.",
)];

/// Every `#NN`–`#NNNN` in `text`, with the byte offset it starts at.
///
/// Two digits minimum so a `#` followed by one digit — far more likely
/// to be prose than a reference — does not trip it, and four maximum so
/// a long digit run is not silently truncated into a match.
fn issue_refs(text: &str) -> Vec<(usize, String)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    for (i, _) in text.match_indices('#') {
        let digits: String = bytes[i + 1..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .map(|b| *b as char)
            .collect();
        if (2..=4).contains(&digits.len()) {
            found.push((i, format!("#{digits}")));
        }
    }
    found
}

#[test]
fn the_published_surface_cites_no_issue_numbers() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/operator_surface.json");
    let surface = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read the surface snapshot at {}: {e}", path.display()));

    let allowed: Vec<&str> = SANCTIONED.iter().map(|(r, _)| *r).collect();
    let offenders: Vec<(usize, String)> = issue_refs(&surface)
        .into_iter()
        .filter(|(_, r)| !allowed.contains(&r.as_str()))
        .collect();

    if offenders.is_empty() {
        return;
    }

    let mut report = String::from(
        "the operator surface cites issue numbers a consumer cannot resolve.\n\n\
         These strings reach `describe`, `fq ops list` and the MCP tool list. A\n\
         reader there has no access to this repository's issue tracker, so the\n\
         number is a dead end where the sentence itself would have served.\n\n\
         Say what the issue says. `#464` became \"recreate the stream and a\n\
         stored sequence names a different letter\" — which is what the reader\n\
         needed, and what the number never told them.\n\n",
    );
    for (at, r) in &offenders {
        let from = surface[..*at].rfind('"').map(|i| i + 1).unwrap_or(*at);
        let to = surface[*at..]
            .find('"')
            .map(|i| at + i)
            .unwrap_or(surface.len());
        report.push_str(&format!("  {r} in: …{}…\n", &surface[from..to].trim()));
    }
    report.push_str(
        "\nIf one genuinely earns its place — a live caveat about the numbers a\n\
         consumer is reading, in `see #N` form — add it to SANCTIONED in this\n\
         file with the reason. That makes the exception a decision in a diff\n\
         rather than a habit.\n",
    );
    panic!("{report}");
}

/// The allowlist describes reality: an entry nobody uses is a stale
/// exemption, and a reader who trusts it is misled about what the
/// surface says.
#[test]
fn every_sanctioned_reference_is_actually_used() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/operator_surface.json");
    let surface = std::fs::read_to_string(&path).expect("read the surface snapshot");
    let present: Vec<String> = issue_refs(&surface).into_iter().map(|(_, r)| r).collect();

    for (reference, reason) in SANCTIONED {
        assert!(
            present.iter().any(|r| r == reference),
            "SANCTIONED lists {reference} but the surface no longer cites it — \
             remove the entry rather than leaving an exemption for a reference \
             that is gone. Its stated reason was: {reason}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::issue_refs;

    #[test]
    fn a_reference_needs_two_to_four_digits() {
        let found: Vec<String> = issue_refs("see #1 and #50 and #1234 and #99999")
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        // `#1` is prose, `#99999` is too long to be one of ours; the
        // two in between are references.
        assert_eq!(found, vec!["#50".to_string(), "#1234".to_string()]);
    }

    #[test]
    fn a_bare_hash_is_not_a_reference() {
        assert!(issue_refs("shell # comment, C# and #fff").is_empty());
    }
}
