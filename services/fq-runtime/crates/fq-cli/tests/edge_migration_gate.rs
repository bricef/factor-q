//! Phase 4 migration gate: the operator surface's remaining legacy
//! call points, counted.
//!
//! ADR-0006/ADR-0031 move every operator verb off direct runtime
//! access and onto the edge. The inventory
//! (`docs/plans/closed/2026-07-28-phase-4-call-point-inventory.md`)
//! enumerates the call points; this gate makes the remaining count a
//! fact the test suite asserts rather than a claim a reviewer has to
//! re-derive from the diff. A flip that leaves the old path in place
//! as a fallback passes its goldens — it does not pass this.
//!
//! Five legacy paths are counted:
//!
//! * `open_views(` — the CLI opening projection stores for itself.
//!   Its definition counted too, so the terminal state was a clean
//!   zero: the last caller's departure took the helper with it, in
//!   cohort 4.4. The pattern stays because what it guards against is a
//!   helper like it reappearing.
//! * `Views::open(` — the same act one level down, which is how it
//!   survived the helper's deletion: `fq status` opened the stores by
//!   calling the constructor `open_views` had wrapped. Added in cohort
//!   4.4 and emptied by verb 14.
//! * `control_plane::operator::` — reaching into runtime internals
//!   directly instead of invoking a declared op.
//! * `AgentRegistry::load_from_directory` — a client verb loading the
//!   agents directory for itself. Not runtime *internals*, which is
//!   why the first two patterns were blind to it, but the same class
//!   of bug and a worse one: the daemon's registry is hot-swapped by
//!   `fq reload`, so a client that reads the disk answers with
//!   definitions the daemon may never have loaded (plan Phase 4,
//!   verb 9).
//! * `.subscribe` — a client verb holding its own bus subscription
//!   (`EventBus::subscribe`, `subscribe_control_*`). Neither a store
//!   open nor a runtime-internals call, so the first three patterns
//!   were blind to it, and it is the worst of the four: core NATS
//!   drops messages silently when a consumer falls behind and cannot
//!   be resumed, so a verb built on one answers with *some* of the
//!   truth and says nothing about the rest (plan Phase 4, verbs 11
//!   and 4).
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
//! than silently shrinking it (#189), and exempt fixtures in both the
//! forms they take — an inline `#[cfg(test)]` module body, and the
//! sibling `foo/tests.rs` AGENTS.md prescribes. It is duplicated rather
//! than shared because a tripwire that depends on another test's
//! helpers can be disarmed from a distance.

use std::fs;
use std::path::{Path, PathBuf};

/// Legacy paths this gate counts. Substring match on production
/// source lines.
const LEGACY: &[&str] = &[
    "open_views(",
    // Added when `fq invocation resume` moved to the edge, because this
    // gate read ZERO while the client still opened its own NATS
    // connection and did request/reply on a bespoke subject. The list
    // above catches store opens and subscriptions; a client that dials
    // the broker to *ask a question* matched none of them, so the
    // number said the phase was done while a second, unauthenticated
    // path to the daemon was still in use. Same failure the `Views::open`
    // entry was added for, in a shape the patterns did not cover.
    "EventBus::connect(",
    // Added when `open_views` was deleted, because deleting the helper
    // did not delete the habit: `fq status` opens the same stores by
    // calling `Views::open` directly. Counting only the helper would
    // have made this gate report zero while client code still opened a
    // store — a number that reads as "the phase is done" and is not.
    // The daemon's own opens carry the exemption below.
    "Views::open(",
    "control_plane::operator::",
    "AgentRegistry::load_from_directory",
    // The leading dot is load-bearing: it matches `.subscribe(` and
    // `.subscribe_control_down()` while leaving `tracing_subscriber`,
    // `'resubscribe:` and the word "subscription" in prose alone.
    ".subscribe",
];

/// Marker exempting a legitimate daemon-side use, on the line or the
/// line above.
const ALLOW: &str = "allow-runtime-internals:";

/// Unmigrated call points remaining. **This number may only go
/// down.** Every flip decrements it; Phase 4 completes at zero.
/// Raising it means a new legacy call point was introduced — that is
/// a hand edit, visible in the diff, and needs a reason at the merge
/// gate.
///
/// It went 6 -> 7 once, with verb 9: not a regression but a widening
/// of what is counted. Adding `AgentRegistry::load_from_directory`
/// admitted four sites — two daemon-side (marked), verb 9's listing
/// (removed by the flip) and verb 5's in-process `fq trigger`, which
/// is the one that raised the count. That verb runs a whole second
/// execution path in the client, disk registry included, and the plan
/// retires it (decision D-1); a gate that did not count it was
/// under-reporting the backlog.
///
/// It went 7 -> 9 with verb 11, for the same reason and with the same
/// arithmetic worth reading. Verb 11's legacy path was a raw bus
/// subscribe, which none of the three patterns matched — so flipping
/// it would have moved this number *not at all*. Adding `.subscribe`
/// admitted six sites: three daemon-side control listeners in
/// `run_daemon` (marked — a daemon owning its own control-plane
/// subscriptions is the architecture), verb 11's own tail (removed by
/// its flip), and **verb 4's `fq down`, which subscribes twice from
/// client code**. Those two are the rise, and they are a real backlog
/// item the gate had been blind to: `fq down` decides whether a
/// daemon stopped by watching a subscription that drops messages
/// silently and cannot be resumed. Cohort 4.3 flips it.
///
/// It went 9 -> 7 with cohort 4.2's last two flips, which land
/// together and each remove one site.
///
/// Verb 12 (`fq events query`) closes the cohort: `event.list` now
/// answers from the daemon's projection index over the edge, so the
/// verb's `open_views(` is gone. Two `open_views(` calls remain — `fq
/// doctor` (verb 15) and `fq costs` (verb 13) — plus the helper's own
/// definition, which is why the terminal state is zero rather than
/// one: the last caller's departure takes the helper with it.
///
/// What held verb 12 up was not the plumbing. A daemon-backed
/// `event.list` necessarily includes the daemon's own events
/// (`system_startup`, `system_recovery`, …), which the
/// `events_query_*` goldens — seeded into a store no daemon had ever
/// touched — did not contain. Those goldens are now a daemon's world,
/// redacted by value; see the rationale at the fixture in
/// `tests/golden.rs`.
///
/// Verb 7 (`fq dead-letters list`) is the other of the two, and now
/// reads the DeadLetter atom.
///
/// It went 7 -> 4 with cohort 4.3's commands, and the arithmetic is
/// worth reading because two of the three departures are not the
/// verbs you would guess. Three sites went:
///
/// * Verb 5's in-process `fq trigger` — the `AgentRegistry::load_from_directory`
///   that raised this count from 6 to 7 in the first place. Retiring
///   that mode (decision D-1) took the whole second execution path
///   with it: the client's WAL writer, its MCP child processes, its
///   pricing loader and its provider client.
/// * **Both** of verb 4's `fq down` subscribes. It decided whether a
///   daemon had stopped by watching `fq.system.shutdown` on one
///   subscription and worker heartbeats on another — two core-NATS
///   streams that drop messages silently and cannot be resumed, held
///   by a client, to answer a question the daemon can be asked
///   directly. `control.down` is a command on the edge now and the
///   confirmation is the daemon's edge going away.
///
/// Verb 3 (`fq reload`) and verb 6 (`fq trigger --via-nats`) flipped in
/// the same cohort and moved this number **not at all**: their legacy
/// path was a bare `EventBus::connect` plus a publish, which none of
/// the four patterns match. That is the cohort-4.1 lesson repeating
/// (check a verb's legacy path is *counted* before trusting a flip to
/// move the number). It was left unfixed at the time, on the argument
/// that a client connecting to the broker is what
/// `store_open_gate.rs`'s sibling discipline and the Phase-4 acceptance
/// criterion ("`fq-cli` … publishes nothing to NATS") cover, and that
/// both are checked by reading the diff at the merge gate rather than
/// by this count.
///
/// **That argument did not hold.** This gate read zero for the whole of
/// Phase 4 while `fq invocation resume` still dialled the broker and
/// did request/reply on `fq.control.invocation.resume` — a second,
/// unauthenticated path to the daemon, in the one crate whose number
/// was supposed to say there were none. "Checked by reading the diff"
/// is not a check; nobody read it. `EventBus::connect(` is a pattern
/// now, the resume flip removed the last site, and the zero below is
/// finally the zero it always claimed to be.
///
/// It went 4 -> 3 with verb 8, the last non-report call point, and the
/// design question cohort 4.3 deferred is what closed it. Requeue was
/// left alone then because `dead_letter.requeue` is a command over a
/// domain whose key is a raw JetStream sequence, while a receipt names
/// atoms by identity — so flipping it would have decided in passing
/// whether a DeadLetter has an identity (#464, still open).
///
/// It does not, and the flip does not need it to: **what a requeue
/// produces is a trigger**, triggers were named and then made
/// permanent records in the two steps before this one, and the dead
/// letter carries the original's `trigger_id`. So the command keys on
/// that, records the new trigger's `requeued_from`, and its receipt
/// names a Trigger — a reference in a different domain from the one
/// the verb is filed under, which is what was actually happening all
/// along.
///
/// It went 3 -> 0 with cohort 4.4's reports, and the three that went
/// are one departure, not three: `fq costs` became `cost.summary`, `fq
/// doctor` became `control.doctor`, and `open_views` — having no
/// callers left — went with them. That is why the terminal state was
/// always zero rather than one.
///
/// It did **not** go to zero, and the reason is worth stating because
/// the obvious arithmetic says it should have.
///
/// Deleting `open_views` did not delete the habit. `fq status` (verb
/// 14, unflipped) opens the same stores by calling `Views::open`
/// directly, twice — which the original four patterns did not match,
/// and which `store_open_gate.rs` does not match either, since it
/// looks for the three `<Store>::open` spellings. Left as they were,
/// both gates would have read clean while fq-cli still opened a store
/// from client code, and the Phase-4 criterion — "`fq-cli` … opens no
/// store" — would have been unmet under a number that reads as though
/// it were met.
///
/// So `Views::open(` joined the patterns and the count went 3 -> 2
/// rather than 3 -> 0. Note which direction that is. This gate has a
/// standing rule against widening patterns to make the number *fall*;
/// widening one to make it *rise* is the opposite act, and it is what
/// the gate is for. A count that has to be explained away is worth
/// less than one that is simply true.
///
/// It went 2 -> 0 with verb 14, and this zero is the plain one. Both
/// sites were `fq status`, the last client verb that read a store for
/// itself; it asks the daemon for `control.status` now. No client-side
/// code in this crate opens a store, calls runtime internals, loads
/// the agents directory, or holds a bus subscription.
///
/// What the zero asserts from here is a boundary rather than progress,
/// and it is worth being precise about how strong it is. An exemption
/// marker is invisible to this count: someone could mark a new
/// client-side call point and the number would not move. That is why
/// the sibling gate (`store_open_gate.rs`) counts *markers* as well as
/// violations, and why `Views::open` is now on both lists — between
/// them, adding a store open to the client half is loud either way.
/// Phase 5 splits the binary, at which point most of this becomes a
/// fact about which crate a symbol is in rather than a convention.
///
/// As of the resume flip the claim is the whole one: no client-side
/// code in this crate opens a store, calls runtime internals, loads
/// the agents directory, holds a bus subscription, **or connects to
/// the broker**. `fq-cli` no longer depends on `async-nats` at all,
/// which is the fact the count was standing in for.
const REMAINING: usize = 0;

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
