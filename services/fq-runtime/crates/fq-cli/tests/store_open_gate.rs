//! Regression gate for #261: read commands must never reacquire a raw
//! store handle. Every direct `ProjectionStore::open*` /
//! `WorkerStore::open*` / `ControlPlaneStore::open*` in non-test fq-cli
//! source must carry an explicit allow-marker naming why it is not a
//! read path (the daemon, an operator write, the trigger WAL writer).
//!
//! Read handlers go through `open_views()` / `Views`; adding a new
//! direct open without a marker fails this test, and adding a marker is
//! a reviewable, greppable act — the gate makes bypasses loud, not
//! impossible.

const SOURCES: &[(&str, &str)] = &[
    ("main.rs", include_str!("../src/main.rs")),
    ("cmd/project.rs", include_str!("../src/cmd/project.rs")),
    ("cmd/trigger.rs", include_str!("../src/cmd/trigger.rs")),
    ("cmd/events.rs", include_str!("../src/cmd/events.rs")),
    ("cmd/status.rs", include_str!("../src/cmd/status.rs")),
    ("daemon/mod.rs", include_str!("../src/daemon/mod.rs")),
    (
        "cmd/dead_letters.rs",
        include_str!("../src/cmd/dead_letters.rs"),
    ),
    ("cmd/views.rs", include_str!("../src/cmd/views.rs")),
    (
        "cmd/invocation.rs",
        include_str!("../src/cmd/invocation.rs"),
    ),
    ("cmd/workers.rs", include_str!("../src/cmd/workers.rs")),
];

/// Marker a sanctioned direct open must carry on its line or the line
/// above.
const ALLOW: &str = "allow-direct-store-open:";

/// Strip `#[cfg(test)]`-gated `mod` blocks by brace counting, so test
/// fixtures (which seed stores read-write by design) are exempt.
/// Assumes rustfmt-normalised source: the `mod` line follows the
/// attribute, and braces in string literals stay balanced (true of the
/// format strings and JSON fixtures in this file; an imbalance fails
/// loudly as a miscounted span, not a silent pass).
fn strip_test_modules(source: &str) -> Vec<(usize, String)> {
    let mut kept = Vec::new();
    let mut lines = source.lines().enumerate().peekable();
    while let Some((idx, line)) = lines.next() {
        if line.trim() == "#[cfg(test)]"
            && lines
                .peek()
                .is_some_and(|(_, next)| next.trim_start().starts_with("mod "))
        {
            let mut depth: i64 = 0;
            let mut entered = false;
            for (_, body) in lines.by_ref() {
                depth += body.matches('{').count() as i64;
                depth -= body.matches('}').count() as i64;
                if depth > 0 {
                    entered = true;
                }
                if entered && depth == 0 {
                    break;
                }
            }
            continue;
        }
        kept.push((idx + 1, line.to_string()));
    }
    kept
}

#[test]
fn read_handlers_never_open_stores_directly() {
    let production: Vec<_> = SOURCES
        .iter()
        .flat_map(|(path, source)| {
            strip_test_modules(source)
                .into_iter()
                .map(move |(line, text)| (*path, line, text))
        })
        .collect();

    let mut violations = Vec::new();
    let mut sanctioned = 0usize;
    for (i, (path, line_no, line)) in production.iter().enumerate() {
        let is_open = [
            "ProjectionStore::open",
            "WorkerStore::open",
            "ControlPlaneStore::open",
        ]
        .iter()
        .any(|needle| line.contains(needle));
        if !is_open {
            continue;
        }
        let marked = line.contains(ALLOW) || i > 0 && production[i - 1].2.contains(ALLOW);
        if marked {
            sanctioned += 1;
        } else {
            violations.push(format!("  {path}:{line_no}: {}", line.trim()));
        }
    }

    assert!(
        violations.is_empty(),
        "direct store open(s) without an `{ALLOW}` marker — read paths must use \
         `open_views()`/`Views` (#261); if this is genuinely a write/daemon path, add the \
         marker with a reason:\n{}",
        violations.join("\n")
    );

    // The sanctioned set is small and intentional; if this count moves,
    // the diff added or removed a marker — make sure the review saw it.
    assert_eq!(
        sanctioned, 7,
        "sanctioned direct-store-open count changed — update this gate alongside the marker"
    );
}
