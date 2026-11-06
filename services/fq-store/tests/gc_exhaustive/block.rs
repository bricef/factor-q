//! Exhaustive block-protocol twin of the object/manifest checker.
//!
//! The writer drives the same reserve-or-materialize loop as `Repository::put`
//! while the collector drives one candidate through claim → unlink → delete.

use std::collections::{HashSet, VecDeque};

use fq_store::{BlockStore, ContentStore, NameIndex, verify};

use super::StoreBackend;

const BLOCK_BYTES: &[u8] = b"gc-exhaustive-block";

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Proc {
    Writer,
    Collector,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum WPc {
    Reserve,
    ChooseGeneration,
    Write,
    Mint,
    Release,
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

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct State {
    /// `(generation, refcount, available)` rows for the one block hash.
    rows: Vec<(u32, i64, bool)>,
    files: Vec<u32>,
    writer: WPc,
    collector: CPc,
    writer_generation: Option<u32>,
}

struct RunOut {
    state: State,
    violation: Option<(usize, String)>,
}

/// Seed one dead block, with its object already reclaimed, so only the block
/// loop can fire. Then replay one schedule against real store primitives.
async fn run<B: StoreBackend>(backend: &B, sched: &[Proc], buggy: bool) -> RunOut {
    let (_guard, repo) = backend.fresh().await;
    let cid = repo.put("seed", BLOCK_BYTES).await.unwrap();
    let block = repo.content().blocks(&cid).await.unwrap()[0];
    repo.unbind("seed").await.unwrap();
    assert!(repo.index().claim_object(&cid).await.unwrap());
    repo.content().remove(&cid).await.unwrap();
    repo.index().delete_object(&cid).await.unwrap();

    let mut writer = WPc::Reserve;
    let mut collector = CPc::Claim;
    let mut writer_generation = None;
    let mut violation = None;

    for (step, proc) in sched.iter().copied().enumerate() {
        match proc {
            Proc::Writer => match writer {
                WPc::Reserve => match repo.index().reserve_block(&block).await.unwrap() {
                    Some(generation) => {
                        writer_generation = Some(generation);
                        writer = WPc::Release;
                    }
                    None => writer = WPc::ChooseGeneration,
                },
                WPc::ChooseGeneration => {
                    writer_generation = Some(repo.index().next_generation(&block).await.unwrap());
                    writer = WPc::Write;
                }
                WPc::Write => {
                    repo.content()
                        .write_block(&block, writer_generation.unwrap(), BLOCK_BYTES)
                        .await
                        .unwrap();
                    writer = WPc::Mint;
                }
                WPc::Mint => {
                    let generation = writer_generation.unwrap();
                    if repo.index().mint_block(&block, generation).await.unwrap() {
                        writer = WPc::Release;
                    } else {
                        // The production loop retries reserve-or-materialize.
                        writer_generation = None;
                        writer = WPc::Reserve;
                    }
                }
                WPc::Release => {
                    repo.index()
                        .release_block(&block, writer_generation.unwrap())
                        .await
                        .unwrap();
                    writer = WPc::Done;
                }
                WPc::Done => {}
            },
            Proc::Collector => match collector {
                CPc::Claim => {
                    if buggy || repo.index().claim_block(&block, 0).await.unwrap() {
                        collector = CPc::Unlink;
                    } else {
                        collector = CPc::Skipped;
                    }
                }
                CPc::Unlink => {
                    repo.content().remove_block(&block, 0).await.unwrap();
                    collector = CPc::Delete;
                }
                CPc::Delete => {
                    repo.index().delete_block(&block, 0).await.unwrap();
                    collector = CPc::Done;
                }
                CPc::Done | CPc::Skipped => {}
            },
        }

        let oracle = verify::check_index_in_flight(repo.index(), repo.content())
            .await
            .unwrap();
        if !oracle.is_empty() && violation.is_none() {
            violation = Some((step, format!("index invariant: {oracle:#?}")));
        }

        // The general oracle deliberately permits an unreferenced block file to
        // be absent. Here a positive writer reservation is the contract under
        // test: GC must not unlink that generation while the writer relies on it.
        let snapshot = repo.index().snapshot().await.unwrap();
        for row in snapshot.blocks.iter().filter(|row| row.hash == block) {
            if row.refcount > 0
                && !repo
                    .content()
                    .has_block(&block, row.generation)
                    .await
                    .unwrap()
                && violation.is_none()
            {
                violation = Some((
                    step,
                    format!(
                        "collector unlinked reserved block generation {}",
                        row.generation
                    ),
                ));
            }
        }
    }

    let snapshot = repo.index().snapshot().await.unwrap();
    let mut rows: Vec<_> = snapshot
        .blocks
        .iter()
        .filter(|row| row.hash == block)
        .map(|row| (row.generation, row.refcount, row.available))
        .collect();
    rows.sort_unstable();
    let mut files: Vec<_> = repo
        .content()
        .list_stored_blocks()
        .await
        .unwrap()
        .into_iter()
        .filter(|(hash, _, _)| *hash == block)
        .map(|(_, generation, _)| generation)
        .collect();
    files.sort_unstable();

    RunOut {
        state: State {
            rows,
            files,
            writer,
            collector,
            writer_generation,
        },
        violation,
    }
}

fn enabled(state: &State) -> Vec<Proc> {
    let mut enabled = Vec::new();
    if state.writer != WPc::Done {
        enabled.push(Proc::Writer);
    }
    if !matches!(state.collector, CPc::Done | CPc::Skipped) {
        enabled.push(Proc::Collector);
    }
    enabled
}

async fn explore<B: StoreBackend>(
    backend: &B,
    buggy: bool,
) -> (usize, Option<(Vec<Proc>, usize, String)>) {
    let initial = run(backend, &[], buggy).await;
    let mut seen = HashSet::from([initial.state.clone()]);
    let mut queue = VecDeque::from([(initial.state, Vec::new())]);
    let mut first_violation = None;

    while let Some((state, schedule)) = queue.pop_front() {
        for proc in enabled(&state) {
            let mut next_schedule = schedule.clone();
            next_schedule.push(proc);
            let out = run(backend, &next_schedule, buggy).await;
            if let Some((step, message)) = out.violation {
                first_violation.get_or_insert((next_schedule.clone(), step, message));
            }
            if seen.insert(out.state.clone()) {
                queue.push_back((out.state, next_schedule));
            }
        }
    }
    (seen.len(), first_violation)
}

pub(super) async fn assert_clean<B: StoreBackend>(backend: &B) {
    let (states, violation) = explore(backend, false).await;
    assert!(
        violation.is_none(),
        "block reserve/mint vs claim reached a forbidden state: {violation:#?}"
    );
    assert!(
        states >= 8,
        "block exploration was vacuous: {states} states"
    );
    eprintln!("exhaustive block protocol: {states} distinct states, no reserved block unlinked");
}

pub(super) async fn assert_sabotage_reaches_violation<B: StoreBackend>(backend: &B) {
    let (states, violation) = explore(backend, true).await;
    assert!(
        violation.is_some(),
        "block checker did not catch claim-CAS sabotage across {states} states"
    );
    eprintln!("exhaustive block protocol (sabotaged): violation reached in {states} states");
}
