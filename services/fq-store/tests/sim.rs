//! Deterministic-simulation harness for the storage layer (M1c slice 1).
//!
//! A seeded workload of put / delete / simulated-crash steps drives a real
//! `Repository`, and after every step we check two things: a **differential
//! model** (every current name reads back exactly the bytes last written; absent
//! names resolve to nothing) and the **invariant oracle** (`fq_store::verify`).
//! The RNG is a self-contained splitmix64, so a failure reproduces exactly from
//! its printed `seed` and `step`.
//!
//! The "crash" step drops and reopens the index on the same files — the faithful
//! single-process crash model: in-memory state is lost, WAL-committed state
//! survives. Mid-operation crashes (via failpoints) arrive with the GC steps.

use std::collections::BTreeMap;
use std::path::Path;

use fq_store::fs::{ChunkParams, FilesystemStore};
use fq_store::{Repository, SqliteNameIndex, verify};

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
    let store = FilesystemStore::with_params(
        cas.to_path_buf(),
        ChunkParams {
            min: 256,
            avg: 1024,
            max: 4096,
        },
    );
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
        match below(&mut rng, 10) {
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
                repo.delete(name).await.unwrap();
                model.remove(name);
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
