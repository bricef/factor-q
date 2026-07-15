//! Deterministic-simulation harness for the storage layer (M1c).
//!
//! A seeded workload of put / delete / gc / audit / fault-injection / crash steps
//! drives a real `Repository`; after every step we check a **differential model**
//! (every current name reads back exactly the bytes last written; absent names
//! resolve to nothing) and the **invariant oracle** (`fq_store::verify`). The RNG
//! is a self-contained splitmix64, so a failure reproduces exactly from its
//! printed `seed` and `step`.
//!
//! Fault steps inject the crash-orphans the reachability audit exists to recover
//! from — an orphan file (a block written with no row) or a leaked reservation (a
//! reserve with no bind). These are safe leaks, not violations, so the per-step
//! oracle still holds; at the end of each seed a full audit past the grace must
//! restore a fully clean store (L4), proven by a second audit finding nothing.
//!
//! The "crash" step drops and reopens the index on the same files — the faithful
//! single-process crash model: in-memory state is lost, WAL-committed survives.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use fq_store::fs::{ChunkParams, FilesystemStore};
use fq_store::{
    AuditReport, BlockStore, Cid, Collector, NameIndex, ReachabilityAuditor, ReferenceCollector,
    Repository, SqliteNameIndex, verify,
};

fn next_u64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn below(state: &mut u64, n: u64) -> u64 {
    next_u64(state) % n
}

async fn open_repo(cas: &Path, db: &Path) -> Repository<FilesystemStore, SqliteNameIndex> {
    let store = FilesystemStore::with_params(cas.to_path_buf(), ChunkParams::small());
    let index = SqliteNameIndex::open(db).await.unwrap();
    Repository::new(store, index)
}

async fn run_seed(seed: u64, steps: usize) {
    let dir = tempfile::tempdir().unwrap();
    let cas = dir.path().join("cas");
    let db = dir.path().join("index.db");
    std::fs::create_dir_all(&cas).unwrap();
    let mut repo = open_repo(&cas, &db).await;

    let mut rng = seed ^ 0xD1B5_4A32_D192_ED03;
    let mut model: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let names = ["a", "a.b", "a.b.c", "a.x", "d", "d.e"];

    for step in 0..steps {
        match below(&mut rng, 16) {
            0..=5 => {
                let name = names[below(&mut rng, names.len() as u64) as usize];
                // Sometimes reuse an existing value — exercises dedup + aliasing.
                let content: Vec<u8> = if !model.is_empty() && below(&mut rng, 3) == 0 {
                    let vals: Vec<&Vec<u8>> = model.values().collect();
                    vals[below(&mut rng, vals.len() as u64) as usize].clone()
                } else {
                    let len = below(&mut rng, 3000) as usize;
                    (0..len).map(|_| next_u64(&mut rng) as u8).collect()
                };
                repo.put(name, &content).await.unwrap();
                model.insert(name.to_string(), content);
            }
            6..=7 => {
                let name = names[below(&mut rng, names.len() as u64) as usize];
                repo.unbind(name).await.unwrap();
                model.remove(name);
            }
            8..=9 => {
                // Online GC: reclaim what's unreferenced. It must hold the oracle
                // and leave every live name untouched.
                ReferenceCollector.collect(&repo).await.unwrap();
            }
            10..=11 => {
                // Reachability audit past the grace (0 → everything eligible; the
                // workload is at rest between steps, so nothing is truly in
                // flight). It reclaims, reconciles, and reaps, holding the oracle.
                ReachabilityAuditor
                    .audit(&repo, Duration::ZERO)
                    .await
                    .unwrap();
            }
            12..=13 => {
                // Inject a crash-orphan for the audit to recover from: an orphan
                // file (a block written with no row) or a leaked reservation (a
                // reserve with no bind). Both are safe leaks — the oracle holds.
                match below(&mut rng, 2) {
                    0 => {
                        let len = below(&mut rng, 400) as usize;
                        let bytes: Vec<u8> = (0..len).map(|_| next_u64(&mut rng) as u8).collect();
                        repo.content()
                            .write_block(&Cid::of(&bytes), 0, &bytes)
                            .await
                            .unwrap();
                    }
                    _ => {
                        let snapshot = repo.index().snapshot().await.unwrap();
                        if let Some(row) = snapshot.blocks.iter().find(|b| b.available) {
                            repo.index().reserve_block(&row.hash).await.unwrap();
                        }
                    }
                }
            }
            _ => {
                // Simulated crash: committed state must survive a reopen.
                repo = open_repo(&cas, &db).await;
            }
        }

        // Differential model: current names read back exactly; others are gone.
        for (name, content) in &model {
            let got = repo
                .get(name)
                .await
                .unwrap_or_else(|e| panic!("seed={seed} step={step}: get({name}) failed: {e}"));
            assert_eq!(
                &got, content,
                "seed={seed} step={step}: content mismatch for {name}"
            );
        }
        for name in names {
            if !model.contains_key(name) {
                assert!(
                    repo.resolve(name).await.unwrap().is_none(),
                    "seed={seed} step={step}: {name} should be unbound"
                );
            }
        }

        // Invariant oracle.
        let violations = verify::check_index(repo.index(), repo.content())
            .await
            .unwrap();
        assert!(
            violations.is_empty(),
            "seed={seed} step={step}: invariant violations: {violations:#?}"
        );
    }

    // Recovery (L4): after the whole workload — including injected orphan files
    // and leaked reservations — a full audit past the grace must restore a wholly
    // clean store, so a *second* audit finds nothing left to do.
    ReachabilityAuditor
        .audit(&repo, Duration::ZERO)
        .await
        .unwrap();
    let residual = ReachabilityAuditor
        .audit(&repo, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(
        residual,
        AuditReport::default(),
        "seed={seed}: audit did not converge to a clean store"
    );
}

/// Quick by default for the CI gate; crank it for a soak run with
/// `FQ_SIM_SEEDS=500 FQ_SIM_STEPS=200 cargo test --test sim`.
#[tokio::test]
async fn dst_put_delete_crash_holds_invariants() {
    fn env_or(key: &str, default: u64) -> u64 {
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
    let seeds = env_or("FQ_SIM_SEEDS", 24);
    let steps = env_or("FQ_SIM_STEPS", 40) as usize;
    for seed in 0..seeds {
        run_seed(seed, steps).await;
    }
}
