//! Phase 4 migration gate: the operator surface's remaining legacy
//! call points, counted.
//!
//! ADR-0006/ADR-0031 move every operator verb off direct runtime
//! access and onto the edge. The inventory
//! (`docs/plans/active/2026-07-28-phase-4-call-point-inventory.md`)
//! enumerates the call points; this gate makes the remaining count a
//! fact the test suite asserts rather than a claim a reviewer has to
//! re-derive from the diff. A flip that leaves the old path in place
//! as a fallback passes its goldens — it does not pass this.
//!
//! Two legacy paths are counted:
//!
//! * `open_views(` — the CLI opening projection stores for itself.
//!   Its definition counts too, so the terminal state is a clean zero:
//!   the last caller's departure takes the helper with it.
//! * `control_plane::operator::` — reaching into runtime internals
//!   directly instead of invoking a declared op.
//!
//! **Marked uses are exempt.** fq-cli is still both the thin client
//! and the daemon host (the binary split is Phase 5), so the same
//! crate legitimately contains daemon-side code — the edge's own
//! command handlers call runtime internals *by design*, and that is
//! the architecture, not debt. Those carry `allow-runtime-internals:`
//! with a reason. Only unmarked sites are the migration backlog.
//!
//! The scanner deliberately mirrors `store_open_gate.rs`: walk `src/`
//! at runtime so a module split joins the gate automatically rather
//! than silently shrinking it (#189), and strip `#[cfg(test)]` module
//! bodies so fixtures are exempt. It is duplicated rather than shared
//! because a tripwire that depends on another test's helpers can be
//! disarmed from a distance.

use std::fs;
use std::path::{Path, PathBuf};

/// Legacy paths this gate counts. Substring match on production
/// source lines.
const LEGACY: &[&str] = &["open_views(", "control_plane::operator::"];

/// Marker exempting a legitimate daemon-side use, on the line or the
/// line above.
const ALLOW: &str = "allow-runtime-internals:";

/// Unmigrated call points remaining. **This number may only go
/// down.** Every flip decrements it; Phase 4 completes at zero.
/// Raising it means a new legacy call point was introduced — that is
/// a hand edit, visible in the diff, and needs a reason at the merge
/// gate.
const REMAINING: usize = 9;

/// Every `.rs` file under `dir`, recursively, in a stable order.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read source dir") {
            let path = entry.expect("read source dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Strip `#[cfg(test)]`-gated `mod` blocks by brace counting, so unit
/// tests exercising the legacy path during a flip do not hold the
/// count up. Assumes rustfmt-normalised source; an imbalance fails
/// loudly as a miscounted span rather than a silent pass.
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
fn legacy_call_points_only_shrink() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = rust_sources(&src_root);
    assert!(
        !files.is_empty(),
        "no .rs files found under {} — the gate is scanning nothing",
        src_root.display()
    );

    let mut remaining = Vec::new();
    for path in &files {
        let source =
            fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        let rel = path
            .strip_prefix(&src_root)
            .expect("source path is under src/")
            .display()
            .to_string();
        let production = strip_test_modules(&source);
        for (i, (line_no, line)) in production.iter().enumerate() {
            if !LEGACY.iter().any(|needle| line.contains(needle)) {
                continue;
            }
            let marked = line.contains(ALLOW) || i > 0 && production[i - 1].1.contains(ALLOW);
            if !marked {
                remaining.push(format!("  {rel}:{line_no}: {}", line.trim()));
            }
        }
    }

    assert_eq!(
        remaining.len(),
        REMAINING,
        "Phase 4 legacy call-point count changed (expected {REMAINING}, found {}).\n\
         If a flip removed one: lower REMAINING to match — that is the ratchet working.\n\
         If this went up: a new direct runtime access was added to the client. Invoke a \
         declared op instead, or if this is daemon-side code, mark it `{ALLOW} <reason>`.\n\
         Remaining:\n{}",
        remaining.len(),
        remaining.join("\n")
    );
}
