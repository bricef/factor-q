//! Phase 3a/3b — deterministic, state-deduped **exhaustive** interleaving check
//! of the object/manifest GC protocol (ADR-0030 back-off, #173), and the
//! **correctness contract** any store backend must pass (Phase 3b).
//!
//! A deterministic BFS drives the *real* index and content primitives as
//! discrete steps — a writer (`put`: RESERVE → MATERIALIZE → BIND, or an
//! `alias`: RESERVE → BIND) and the collector (CLAIM → UNLINK → DELETE, the
//! object arm of `collect`) — enumerates every interleaving, and asserts the
//! always-invariants ([`verify::check_index_in_flight`]) after **every** step.
//!
//! It dedupes by an **abstract-state projection** (object kind, manifest
//! present, name bound, each process's PC), so it converges to the reachable
//! *state* count, not the far larger *schedule* count, and needs no seeds. Each
//! successor replays its schedule from a fresh store (the store is filesystem +
//! SQLite, not cheaply snapshot-restored), which state-dedup keeps bounded.
//!
//! **Trait-generic (Phase 3b).** The check is generic over any
//! [`StoreBackend`] — a `BlockStore` + `NameIndex` pair — so it is the reusable
//! contract for future backends: implement `StoreBackend::fresh` and the same
//! exhaustive bar applies. Only the local `FsSqlite` backend exists today; the
//! GC protocol lives at the local index + manifest layer, so — unlike the
//! functional `conformance.rs`, which runs content conformance over `tarpc` —
//! there is no over-the-wire backend to check here (the `tarpc` service is
//! content-only: no `write_object`, no `NameIndex`).
//!
//! Each scenario is checked twice: back-off is clean across the whole state
//! space, and a **sabotaged collector** (unlink without the claim CAS — reverting
//! the fix's core protection) *reaches* S1-obj, proving the check is not vacuous.
//!
//! Fidelity note: the step sequences mirror `Repository::put` / `Repository::bind`
//! and the object arm of `ReferenceCollector::collect`. A `Crash` process (#248)
//! halts the writer mid-flight running no cleanup — a process death that leaks its
//! reservation — at every reserved-but-unbound point and against every collector
//! interleaving; the reachability audit must then recover a clean, at-rest store
//! (the object reconcile, #243). Still deferred (#253,
//! `storage-concurrency-verification.md`): a second concurrent writer, the block
//! arm, and per-step error injection — the `Proc`/step-machine extends to all.

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use fq_store::fs::{ChunkParams, FilesystemStore};
use fq_store::{
    AuditReport, BlockStore, Cid, ContentStore, NameIndex, ReachabilityAuditor, Repository,
    SqliteNameIndex, verify,
};

const CONTENT: &[u8] = b"gc-exhaustive-object"; // small: one block

/// A store backend the exhaustive check runs against — the reusable contract.
/// Implement `fresh` (a new, empty store) and the interleaving bar applies.
trait StoreBackend {
    type Content: BlockStore;
    type Index: NameIndex;
    type Guard;
    async fn fresh(&self) -> (Self::Guard, Repository<Self::Content, Self::Index>);
}

/// The local filesystem-CAS + SQLite-index backend.
struct FsSqlite;

impl StoreBackend for FsSqlite {
    type Content = FilesystemStore;
    type Index = SqliteNameIndex;
    type Guard = tempfile::TempDir;

    async fn fresh(&self) -> (Self::Guard, Repository<Self::Content, Self::Index>) {
        let dir = tempfile::tempdir().unwrap();
        let cas = dir.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let store = FilesystemStore::with_params(cas, ChunkParams::small());
        let index = SqliteNameIndex::open(dir.path().join("index.db"))
            .await
            .unwrap();
        (dir, Repository::new(store, index))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Proc {
    Writer,
    Collector,
    /// A process death mid-put: halt the writer where it stands, running no
    /// cleanup — a crash does not release its reservation, and that leak is
    /// exactly what the audit must recover. Enabled only while the writer is
    /// reserved-but-unbound; committed state is what survives.
    Crash,
}

/// The writer program: a re-`put` (writes the manifest) or an `alias` (trusts
/// the existing manifest — no `MATERIALIZE`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum WriterKind {
    Put,
    Alias,
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

/// The object's abstract kind. `Reserved(rc)` carries the (small) refcount; a
/// reserved/live object is always available (claim needs refcount 0), so
/// `available` is implicit.
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
    /// The writer died mid-flight (a `Crash` step ran). Distinguishes a
    /// crash-leaked terminal state from a cleanly completed one, for dedup.
    crashed: bool,
}

struct RunOut {
    state: AbstractState,
    /// The first always-invariant violation observed, with the step index.
    violation: Option<(usize, Vec<verify::Violation>)>,
    /// Present iff the writer crashed: the outcome of the post-crash recovery
    /// audit — whether the crash left a real at-rest leak, and whether the audit
    /// then restored a clean, converged store.
    recovery: Option<Recovery>,
}

#[derive(Clone, Copy)]
struct Recovery {
    /// The crashed store failed the strict at-rest oracle *before* recovery —
    /// there really was a leaked reservation to recover (non-vacuity).
    dirty_before: bool,
    /// *After* the audit: the at-rest oracle is clean and a second audit is a
    /// no-op (the store converged).
    clean_after: bool,
}

/// Run `sched` from a fresh store, one process step per entry, asserting the
/// always-invariants after every step. Generic over the backend.
async fn run<B: StoreBackend>(
    backend: &B,
    sched: &[Proc],
    kind: WriterKind,
    buggy: bool,
) -> RunOut {
    let (_guard, repo) = backend.fresh().await;

    // Seed a dead-but-uncollected object with its manifest present, and hold its
    // block reservations as a mid-flight writer would (so the collector's block
    // arm never fires — this isolates the object/manifest race).
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
    let mut crashed = false;
    let mut violation = None;

    for (i, &p) in sched.iter().enumerate() {
        match p {
            Proc::Writer => match w {
                WPc::Reserve => {
                    // `put` creates an absent object (it writes the manifest); an
                    // `alias` does not (it trusts the manifest) and skips
                    // MATERIALIZE.
                    let create = matches!(kind, WriterKind::Put);
                    match repo.index().reserve_object(&cid, create).await.unwrap() {
                        Some(prev) => {
                            w_prev = prev;
                            w = match kind {
                                WriterKind::Put => WPc::Write,
                                WriterKind::Alias => WPc::Bind,
                            };
                        }
                        // Refused (claimed, or absent-and-aliasing). `put` retries
                        // internally (PC unchanged); `alias` returns `Conflict` and
                        // is done.
                        None => {
                            if matches!(kind, WriterKind::Alias) {
                                w = WPc::Done;
                            }
                        }
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
            Proc::Crash => {
                // Process death: halt the writer, run no cleanup. Every primitive
                // committed in its own txn, so the surviving store is exactly what
                // we hold — no reopen is needed at this layer (the DST covers the
                // on-disk reopen); the leaked reservation persists for recovery.
                crashed = true;
                w = WPc::Done;
            }
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
    let state = AbstractState {
        obj,
        manifest,
        name_bound,
        w,
        w_prev,
        c,
        crashed,
    };

    // After a crash the online collector cannot reclaim a still-reserved leak
    // (claim needs refcount 0) — the reachability audit is the backstop. It must
    // reconcile the leaked reservation past the grace and reclaim, restoring a
    // clean at-rest store (a second audit then finds nothing). The strict at-rest
    // oracle is dirty *before* (there is a real leak) and clean *after*.
    let recovery = if crashed {
        let before = verify::check_index(repo.index(), repo.content())
            .await
            .unwrap();
        ReachabilityAuditor
            .audit(&repo, Duration::ZERO)
            .await
            .unwrap();
        let residual = ReachabilityAuditor
            .audit(&repo, Duration::ZERO)
            .await
            .unwrap();
        let after = verify::check_index(repo.index(), repo.content())
            .await
            .unwrap();
        Some(Recovery {
            dirty_before: !before.is_empty(),
            clean_after: after.is_empty() && residual == AuditReport::default(),
        })
    } else {
        None
    };

    RunOut {
        state,
        violation,
        recovery,
    }
}

/// Project the object's concrete state onto its abstract kind, without a schema
/// change: a refcount > 0 object is always available (claim needs refcount 0),
/// and a refcount-0 object's flag comes from `claimable_objects`.
async fn object_kind<C: BlockStore, N: NameIndex>(repo: &Repository<C, N>, cid: &Cid) -> Obj {
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

fn enabled(s: &AbstractState, allow_crash: bool) -> Vec<Proc> {
    let mut v = Vec::new();
    if s.w != WPc::Done {
        v.push(Proc::Writer);
    }
    if s.c != CPc::Done && s.c != CPc::Skipped {
        v.push(Proc::Collector);
    }
    // A crash matters only once the writer has reserved (there is a reservation to
    // leak) and before it binds (after bind the put is complete — nothing to
    // recover): exactly WPc::Write / WPc::Bind. Fires at most once per schedule.
    if allow_crash && !s.crashed && matches!(s.w, WPc::Write | WPc::Bind) {
        v.push(Proc::Crash);
    }
    v
}

/// Aggregate outcome of a full BFS exploration.
struct Explored {
    states: usize,
    first_violation: Option<(Vec<Proc>, usize, Vec<verify::Violation>)>,
    /// Crash schedules run, how many left a real at-rest leak, and the first
    /// schedule (if any) whose post-crash recovery failed.
    crashes: usize,
    crashes_dirty: usize,
    recovery_failure: Option<Vec<Proc>>,
}

/// Fold one run's outcome into the aggregate.
fn fold(ex: &mut Explored, out: &RunOut, sched: &[Proc]) {
    if let Some((i, v)) = &out.violation {
        ex.first_violation
            .get_or_insert_with(|| (sched.to_vec(), *i, v.clone()));
    }
    if let Some(r) = &out.recovery {
        ex.crashes += 1;
        if r.dirty_before {
            ex.crashes_dirty += 1;
        }
        if !r.clean_after {
            ex.recovery_failure.get_or_insert_with(|| sched.to_vec());
        }
    }
}

/// BFS over abstract states; every successor replays its schedule from a fresh
/// store. Checks every run's outcome (violation, and recovery for crash steps),
/// returning the distinct-state count and the aggregated findings.
async fn explore<B: StoreBackend>(
    backend: &B,
    kind: WriterKind,
    buggy: bool,
    allow_crash: bool,
) -> Explored {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::new();
    let mut ex = Explored {
        states: 0,
        first_violation: None,
        crashes: 0,
        crashes_dirty: 0,
        recovery_failure: None,
    };

    let init = run(backend, &[], kind, buggy).await;
    fold(&mut ex, &init, &[]);
    seen.insert(init.state.clone());
    queue.push_back((init.state, Vec::<Proc>::new()));

    while let Some((state, sched)) = queue.pop_front() {
        for p in enabled(&state, allow_crash) {
            let mut sched2 = sched.clone();
            sched2.push(p);
            let out = run(backend, &sched2, kind, buggy).await;
            fold(&mut ex, &out, &sched2);
            if seen.insert(out.state.clone()) {
                queue.push_back((out.state, sched2));
            }
        }
    }
    ex.states = seen.len();
    ex
}

/// The clean bar: no interleaving of `kind` vs the collector reaches S1-obj.
async fn assert_clean<B: StoreBackend>(backend: &B, kind: WriterKind, label: &str) {
    let ex = explore(backend, kind, false, false).await;
    assert!(
        ex.first_violation.is_none(),
        "{label}: S1-obj is reachable under back-off — the fix is incomplete: {:#?}",
        ex.first_violation
    );
    assert!(
        ex.states >= 8,
        "{label}: expected a non-trivial state space, explored only {}",
        ex.states
    );
    eprintln!(
        "exhaustive {label}: {} distinct states, no reachable S1-obj",
        ex.states
    );
}

/// The non-vacuity meta-check: with the claim CAS removed, S1-obj IS reachable.
async fn assert_sabotage_reaches_s1<B: StoreBackend>(backend: &B, kind: WriterKind, label: &str) {
    let ex = explore(backend, kind, true, false).await;
    assert!(
        ex.first_violation.is_some(),
        "{label}: the sabotaged collector should reach S1-obj, but found none across {} states",
        ex.states
    );
    eprintln!(
        "exhaustive {label} (sabotaged): reached S1-obj ({} states)",
        ex.states
    );
}

/// The crash-recovery bar: a writer that dies reserved-but-unbound — at every
/// such point, against every collector interleaving — leaves a store the audit
/// restores to clean and at-rest. Non-vacuous: every such crash really did leak
/// (the strict at-rest oracle flagged it before recovery), and the always-
/// invariants hold at every step of the crash interleavings too.
async fn assert_crash_recovers<B: StoreBackend>(backend: &B, kind: WriterKind, label: &str) {
    let ex = explore(backend, kind, false, true).await;
    assert!(
        ex.first_violation.is_none(),
        "{label}: an always-invariant was violated during a crash interleaving: {:#?}",
        ex.first_violation
    );
    assert!(
        ex.crashes > 0,
        "{label}: no crash schedules were explored — the Crash process never fired"
    );
    assert_eq!(
        ex.crashes_dirty, ex.crashes,
        "{label}: a crash left no at-rest leak — the recovery check would be vacuous"
    );
    assert!(
        ex.recovery_failure.is_none(),
        "{label}: a crash-leaked reservation was not recovered by the audit: {:#?}",
        ex.recovery_failure
    );
    eprintln!(
        "exhaustive {label} (crash): {} crash schedules recovered to a clean at-rest store ({} states)",
        ex.crashes, ex.states
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhaustive_put_backoff_has_no_reachable_s1() {
    assert_clean(&FsSqlite, WriterKind::Put, "put").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhaustive_put_sabotaged_reaches_s1() {
    assert_sabotage_reaches_s1(&FsSqlite, WriterKind::Put, "put").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhaustive_alias_backoff_has_no_reachable_s1() {
    assert_clean(&FsSqlite, WriterKind::Alias, "alias").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhaustive_alias_sabotaged_reaches_s1() {
    assert_sabotage_reaches_s1(&FsSqlite, WriterKind::Alias, "alias").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhaustive_put_crash_is_recovered_by_audit() {
    assert_crash_recovers(&FsSqlite, WriterKind::Put, "put").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhaustive_alias_crash_is_recovered_by_audit() {
    assert_crash_recovers(&FsSqlite, WriterKind::Alias, "alias").await;
}
