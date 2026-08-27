//! Regression gate for #261: read commands must never reacquire a raw
//! store handle. Every direct `ProjectionStore::open*` /
//! `WorkerStore::open*` / `ControlPlaneStore::open*` / `Views::open*`
//! in non-test fq-daemon source must carry an explicit allow-marker
//! naming why it is not a read path (the daemon, an operator write,
//! the trigger WAL writer).
//!
//! Adding a new direct open without a marker fails this test, and
//! adding a marker is a reviewable, greppable act — the gate makes
//! bypasses loud, not impossible.
//!
//! **What a read handler does instead has changed under this gate.**
//! The rule used to be "go through `open_views()` / `Views`": the CLI
//! was a formatter over a read layer it opened for itself, and the
//! bypass being caught was a *second* way of doing that. There is no
//! `open_views()` any more (cohort 4.4 took its last two callers), and
//! the remedy is now to invoke a declared op over the edge — the
//! daemon owns the stores. `Views` is what a *handler* reads, on the
//! other side of that call.
//!
//! `Views::open(` is on the list below because verb 14 put it there,
//! which is the promise the previous version of this paragraph made.
//! It is a store open one level down — `Views::open` opens all three —
//! so a gate blind to it was blind to `fq status`, the last client
//! verb that used it. Added and emptied in the same change, so the
//! gate never goes red for work someone else has to do.
//!
//! Adding it made two daemon-side opens newly visible, and they are
//! marked in place rather than moved: `run_daemon` sits exactly on its
//! function-size budget, so a marker on its own line would breach a
//! ratchet to satisfy a lint. Those two lines carry both gates'
//! markers, which reads as the marker list it is.
//!
//! Sources are discovered by walking `src/` at runtime, so a file added
//! to the tree joins the gate automatically. A compile-time embed (the
//! old `include_str!` of main.rs) or a hardcoded file list would let a
//! module split silently shrink the scan (#189) — and embedding `.rs`
//! sources is itself rejected by `just lint-sources`. Fixtures are exempt
//! in both the forms they take: an inline `#[cfg(test)]` module body, and
//! the sibling `foo/tests.rs` AGENTS.md prescribes.

use std::fs;
use std::path::{Path, PathBuf};

/// Marker a sanctioned direct open must carry on its line or the line
/// above.
const ALLOW: &str = "allow-direct-store-open:";

/// True when `path` is the test half of a module split — `foo/tests.rs`
/// beside a `foo.rs` that declares `#[cfg(test)] mod tests;`.
///
/// AGENTS.md puts unit tests in a sibling file so a module's production
/// code is not buried under its own tests, and `fq-lint` resolves the same
/// declaration to exclude them from both size budgets. This gate has to
/// agree: that code is `#[cfg(test)]` either way, and a scan that counted
/// it would report a module split (#189) as a ratchet regression purely
/// because the fixtures changed shape. The declaration is what is checked,
/// not the filename, so a `tests.rs` nobody declares still gets scanned.
fn is_sibling_test_file(path: &Path) -> bool {
    if path.file_name().is_none_or(|name| name != "tests.rs") {
        return false;
    }
    path.parent()
        .map(|dir| dir.with_extension("rs"))
        .and_then(|declaring| fs::read_to_string(declaring).ok())
        .is_some_and(|src| src.contains("#[cfg(test)]\nmod tests;"))
}

/// Every `.rs` file under `dir`, recursively, in a stable order.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read source dir") {
            let path = entry.expect("read source dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && !is_sibling_test_file(&path)
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

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
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = rust_sources(&src_root);
    assert!(
        !files.is_empty(),
        "no .rs files found under {} — the gate is scanning nothing",
        src_root.display()
    );

    let mut violations = Vec::new();
    let mut sanctioned = 0usize;
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
            let is_open = [
                "ProjectionStore::open",
                "WorkerStore::open",
                "ControlPlaneStore::open",
                // The read layer's constructor: it opens all three, so
                // a scan that skipped it could not see the last client
                // verb that reached for a store (plan Phase 4, verb 14).
                "Views::open",
            ]
            .iter()
            .any(|needle| line.contains(needle));
            if !is_open {
                continue;
            }
            let marked = line.contains(ALLOW) || i > 0 && production[i - 1].1.contains(ALLOW);
            if marked {
                sanctioned += 1;
            } else {
                violations.push(format!("  {rel}:{line_no}: {}", line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "direct store open(s) without an `{ALLOW}` marker — a client verb asks the daemon \
         for what it needs by invoking a declared op (#261); if this is genuinely a \
         write/daemon path, add the marker with a reason:\n{}",
        violations.join("\n")
    );

    // The sanctioned set is small and intentional; if this count moves,
    // the diff added or removed a marker — make sure the review saw it.
    //
    // It went 5 -> 4 with cohort 4.3, and the marker it lost is the one
    // worth naming: the in-process `fq trigger` opened the worker WAL
    // *as a writer*, from the client, sanctioned on the grounds that it
    // was "a one-shot worker". Retiring that mode (decision D-1) took
    // that one.
    //
    // 4 -> 3 retires the last exemption that was not the runtime opening
    // its own stores: `fq workers prune`  allow-dead-command: retired verb, named as history opened the control-plane store
    // to delete stale registration rows. Reclaiming those rows is a
    // daemon retention sweep now — the system should not depend on
    // operator remediations to work normally — so the write moved inside
    // the daemon and the verb was deleted rather than transplanted.
    //
    // The three that remain are all inside `run_daemon`: the runtime
    // opening its own projection, control-plane, and worker stores,
    // which is the architecture rather than a concession. A fourth
    // marker appearing means someone re-opened a store from the client
    // side, and that is the thing this gate exists to make loud.
    //
    // Cohort 4.4 moved this number **not at all**, which is worth
    // stating because it is the cohort that emptied the *other* gate:
    // `fq costs` and `fq doctor` reached their stores through
    // `open_views`, never through a `<Store>::open`, so retiring both
    // verbs' local reads removed nothing this scan was counting. The
    // exemptions here were always about the daemon; they still are.
    //
    // 3 -> 5 with verb 14, and NOT because anything opened a store.
    // Nothing did: the verb that used to (`fq status`) stopped, which
    // is the change. The two new entries are the daemon's own
    // `Views::open` calls — the read service's and the edge's — which
    // this scan could not see until `Views::open` joined the spellings
    // above, and which were already marked as daemon-side under the
    // sibling gate's vocabulary. Both markers now sit on those two
    // lines. So the rise is coverage, not concession; the number of
    // places a store is opened outside the daemon is still zero, and
    // that is the number this gate is really about.
    //
    // 5 -> 4 when `ReadService` retired: one of those two daemon-side
    // `Views::open` calls was the read service's, and it went with the
    // service. The edge's remains, and it is now the only read path
    // into the stores — which is the shape the whole migration was
    // for. A ratchet that only ever tightens is allowed to tighten.
    assert_eq!(
        sanctioned, 4,
        "sanctioned direct-store-open count changed — update this gate alongside the marker"
    );
}
