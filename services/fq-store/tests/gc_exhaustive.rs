//! Phase 3a — deterministic, state-deduped **exhaustive** interleaving check of
//! the object/manifest GC protocol (ADR-0030 back-off, #173).
//!
//! This is the runtime counterpart of the TLA⁺ model: it drives the *real* index
//! and content primitives as discrete steps, enumerates every interleaving of a
//! writer (a re-`put` of a dead object) and the collector, and asserts the
//! always-invariants ([`verify::check_index_in_flight`]) after **every** step.
//!
//! It dedupes by an **abstract-state projection** — the object's kind
//! (absent / dead-available / claimed / reserved), whether its manifest is
//! present, whether the name is bound, and each process's program counter — so
//! the exploration converges to the reachable-*state* count (the TLA model's
//! order) rather than the far larger *schedule* count, and needs no seeds. Each
//! successor is reached by replaying its schedule from a fresh store (the store
//! is filesystem + SQLite, not cheaply snapshot-restored), which state-dedup
//! keeps bounded.
//!
//! Two checks: back-off is clean across the whole state space, and a **sabotaged
//! collector** (unlink without the claim CAS — reverting the fix's core
//! protection) *reaches* S1-obj, proving the check is not vacuous. See
//! `docs/design/committed/storage-concurrency-verification.md`.
//!
//! Fidelity note: the writer/collector step sequences here mirror
//! `Repository::put` (RESERVE → MATERIALIZE → BIND) and the object arm of
//! `ReferenceCollector::collect` (CLAIM → UNLINK → DELETE). They call the same
//! primitives in the same order; keep them in sync if those change.

use std::collections::{HashSet, VecDeque};
use std::path::Path;

use fq_store::fs::{ChunkParams, FilesystemStore};
use fq_store::{BlockStore, Cid, ContentStore, NameIndex, Repository, SqliteNameIndex, verify};

const CONTENT: &[u8] = b"gc-exhaustive-object"; // small: one block

type Repo = Repository<FilesystemStore, SqliteNameIndex>;

async fn open_repo(dir: &Path) -> Repo {
    let cas = dir.join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let store = FilesystemStore::with_params(cas, ChunkParams::small());
    let index = SqliteNameIndex::open(dir.join("index.db")).await.unwrap();
    Repository::new(store, index)
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Proc {
    Writer,
    Collector,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum WPc {
    Reserve,
    Write,
    Bind,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum CPc {
    Claim,
    Unlink,
    Delete,
    Done,
    Skipped,
}

/// The object's abstract kind. `Reserved(rc)` carries the (small) refcount so a
/// reserved object is distinguished from a live one; a reserved/live object is
/// always available (claim needs refcount 0), so `available` is implicit.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Obj {
    Absent,
    DeadAvailable,
    Claimed,
    Reserved(i64),
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct AbstractState {
    obj: Obj,
    manifest: bool,
    name_bound: bool,
    w: WPc,
    w_prev: i64,
    c: CPc,
}

struct RunOut {
    state: AbstractState,
    /// The first always-invariant violation observed, with the step index.
    violation: Option<(usize, Vec<verify::Violation>)>,
}

/// Run `sched` from a fresh store, one process step per entry, asserting the
/// always-invariants after every step. Returns the final abstract state and the
/// first violation seen (if any).
async fn run(sched: &[Proc], buggy: bool) -> RunOut {
    let dir = tempfile::tempdir().unwrap();
    let repo = open_repo(dir.path()).await;

    // Seed a dead-but-uncollected object with its manifest present, and hold its
    // block reservations as a mid-flight put would (so the collector's block arm
    // never fires — this isolates the object/manifest race).
    let cid = repo.put("a", CONTENT).await.unwrap();
    repo.unbind("a").await.unwrap();
    let mut reserved = Vec::new();
    for b in repo.content().blocks(&cid).await.unwrap() {
        let g = repo
            .index()
            .reserve_block(&b)
            .await
            .unwrap()
            .expect("seed: block available");
        reserved.push((b, g));
    }
    let blocks: Vec<(Cid, u32, u64)> = reserved
        .iter()
        .map(|(h, g)| (*h, *g, CONTENT.len() as u64))
        .collect();

    let mut w = WPc::Reserve;
    let mut c = CPc::Claim;
    let mut w_prev = 0i64;
    let mut violation = None;

    for (i, &p) in sched.iter().enumerate() {
        match p {
            Proc::Writer => match w {
                WPc::Reserve => {
                    // `None` means claimed — back off, retry (PC unchanged).
                    if let Some(prev) = repo.index().reserve_object(&cid).await.unwrap() {
                        w_prev = prev;
                        w = WPc::Write;
                    }
                }
                WPc::Write => {
                    repo.content()
                        .write_object(&cid, CONTENT.len() as u64, &blocks)
                        .await
                        .unwrap();
                    w = WPc::Bind;
                }
                WPc::Bind => {
                    repo.index()
                        .bind("b", &cid, &reserved, w_prev)
                        .await
                        .unwrap();
                    w = WPc::Done;
                }
                WPc::Done => {}
            },
            Proc::Collector => match c {
                CPc::Claim => {
                    if buggy {
                        // Sabotage: unlink without claiming — reverts the fix's
                        // core protection (the collector no longer guards the
                        // manifest behind a claim CAS).
                        c = CPc::Unlink;
                    } else if repo.index().claim_object(&cid).await.unwrap() {
                        c = CPc::Unlink;
                    } else {
                        c = CPc::Skipped; // a writer reserved first — leave it alone
                    }
                }
                CPc::Unlink => {
                    repo.content().remove(&cid).await.unwrap();
                    c = CPc::Delete;
                }
                CPc::Delete => {
                    repo.index().delete_object(&cid).await.unwrap();
                    c = CPc::Done;
                }
                CPc::Done | CPc::Skipped => {}
            },
        }

        let v = verify::check_index_in_flight(repo.index(), repo.content())
            .await
            .unwrap();
        if !v.is_empty() && violation.is_none() {
            violation = Some((i, v));
        }
    }

    let obj = object_kind(&repo, &cid).await;
    let manifest = repo.content().has(&cid).await.unwrap();
    let name_bound = repo.index().resolve("b").await.unwrap() == Some(cid);
    RunOut {
        state: AbstractState {
            obj,
            manifest,
            name_bound,
            w,
            w_prev,
            c,
        },
        violation,
    }
}

/// Project the object's concrete state onto its abstract kind. `available` is
/// derived without a schema change: a refcount > 0 object is always available
/// (claim needs refcount 0), and a refcount-0 object's flag comes from
/// `claimable_objects`.
async fn object_kind(repo: &Repo, cid: &Cid) -> Obj {
    let snap = repo.index().snapshot().await.unwrap();
    match snap.objects.iter().find(|(o, _)| o == cid) {
        None => Obj::Absent,
        Some((_, rc)) if *rc > 0 => Obj::Reserved(*rc),
        Some(_) => {
            let claimable = repo.index().claimable_objects().await.unwrap();
            match claimable.iter().find(|(c, _)| c == cid) {
                Some((_, false)) => Obj::Claimed,
                _ => Obj::DeadAvailable,
            }
        }
    }
}

fn enabled(s: &AbstractState) -> Vec<Proc> {
    let mut v = Vec::new();
    if s.w != WPc::Done {
        v.push(Proc::Writer);
    }
    if s.c != CPc::Done && s.c != CPc::Skipped {
        v.push(Proc::Collector);
    }
    v
}

/// BFS over abstract states; every successor replays its schedule from a fresh
/// store. Returns the distinct-state count and the first violation found.
async fn explore(buggy: bool) -> (usize, Option<(Vec<Proc>, usize, Vec<verify::Violation>)>) {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::new();
    let mut first_violation = None;

    let init = run(&[], buggy).await;
    if let Some((i, v)) = init.violation {
        first_violation.get_or_insert((Vec::new(), i, v));
    }
    seen.insert(init.state.clone());
    queue.push_back((init.state, Vec::<Proc>::new()));

    while let Some((state, sched)) = queue.pop_front() {
        for p in enabled(&state) {
            let mut sched2 = sched.clone();
            sched2.push(p);
            let out = run(&sched2, buggy).await;
            if let Some((i, v)) = out.violation {
                first_violation.get_or_insert((sched2.clone(), i, v));
            }
            if seen.insert(out.state.clone()) {
                queue.push_back((out.state, sched2));
            }
        }
    }
    (seen.len(), first_violation)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhaustive_backoff_has_no_reachable_s1() {
    let (states, violation) = explore(false).await;
    assert!(
        violation.is_none(),
        "S1-obj is reachable under back-off — the fix is incomplete: {violation:#?}"
    );
    // Sanity: a real interleaving space was explored, not a degenerate one.
    assert!(
        states >= 8,
        "expected a non-trivial state space, explored only {states}"
    );
    eprintln!("exhaustive back-off: {states} distinct states, no reachable S1-obj");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhaustive_sabotaged_collector_reaches_s1() {
    // Meta-check: with the claim CAS removed the checker MUST find S1-obj —
    // proving the clean result above is a real guarantee, not a vacuous pass.
    let (states, violation) = explore(true).await;
    assert!(
        violation.is_some(),
        "the sabotaged collector should reach S1-obj, but the checker found none across {states} states"
    );
    eprintln!("exhaustive sabotaged: reached S1-obj ({states} states explored)");
}
