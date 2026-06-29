//! Baseline throughput benchmarks for the filesystem CAS backend.
//!
//! Not a CI gate — run on demand with `cargo bench`. These establish a
//! baseline (and guard against regressions) for put/get/get_range across a
//! spread of sizes, so "is the CAS the bottleneck?" can be answered with a
//! number rather than a guess. (It almost certainly isn't, next to LLM calls.)

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use fq_store::ContentStore;
use fq_store::fs::FilesystemStore;
use tokio::runtime::Runtime;

const SIZES: &[usize] = &[1024, 64 * 1024, 1024 * 1024, 8 * 1024 * 1024];

/// Deterministic pseudo-random bytes (xorshift) so blocks are realistically
/// unique — an all-constant fill would dedup to nothing and overstate speed.
fn data(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size + 8);
    let mut x = 0x2545_f491_4f6c_dd1du64;
    while out.len() < size {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(size);
    out
}

fn put(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("put");
    for &size in SIZES {
        let bytes = data(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &bytes, |b, bytes| {
            // Fresh store per iteration: `put` is idempotent, so a reused store
            // would measure the dedup fast-path instead of a real write.
            b.iter_batched(
                || tempfile::tempdir().unwrap(),
                |dir| {
                    rt.block_on(async {
                        FilesystemStore::new(dir.path()).put(bytes).await.unwrap()
                    })
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn get(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("get");
    for &size in SIZES {
        let dir = tempfile::tempdir().unwrap();
        let store = FilesystemStore::new(dir.path());
        let cid = rt.block_on(store.put(&data(size))).unwrap();
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &cid, |b, cid| {
            b.iter(|| rt.block_on(async { store.get(cid).await.unwrap() }));
        });
    }
    group.finish();
}

fn get_range(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = FilesystemStore::new(dir.path());
    let size = 8 * 1024 * 1024usize;
    let cid = rt.block_on(store.put(&data(size))).unwrap();
    let span = 64 * 1024u64;
    let mut group = c.benchmark_group("get_range");
    group.throughput(Throughput::Bytes(span));
    group.bench_function("64KiB_at_midpoint", |b| {
        b.iter(|| {
            rt.block_on(async { store.get_range(&cid, size as u64 / 2, span).await.unwrap() })
        });
    });
    group.finish();
}

criterion_group!(benches, put, get, get_range);
criterion_main!(benches);
