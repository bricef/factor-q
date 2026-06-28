# Implementing a `ContentStore` backend

The content store (ADR-0023 layer 1) is the bedrock of factor-q's storage
foundation, so every backend is held to one shared bar: it must pass the
**conformance suite**. This guide walks through implementing the
[`ContentStore`] trait and proving an implementation against that suite —
the filesystem backend (`fq_store::fs::FilesystemStore`) is the worked
reference.

The payoff of the suite: the correctness guarantees are written once, against
the trait, so a new backend (S3, a database, an in-memory cache) re-runs the
*exact same* property tests with a single macro invocation.

## The trait

```rust
#[async_trait]
pub trait ContentStore: Send + Sync {
    async fn put(&self, content: &[u8]) -> Result<Cid>;
    async fn get(&self, cid: &Cid) -> Result<Vec<u8>>;
    async fn get_range(&self, cid: &Cid, offset: u64, len: u64) -> Result<Vec<u8>>;
    async fn has(&self, cid: &Cid) -> Result<bool>;
    async fn size(&self, cid: &Cid) -> Result<u64>;
}
```

## The contract (what the conformance suite enforces)

- **Content-addressed.** `put(content)` returns `Cid::of(content)` — the
  BLAKE3 hash of the bytes. Any party can derive a `Cid` from content alone;
  the store does not assign ids.
- **Idempotent.** Storing identical content again returns the same `Cid` and
  must not duplicate storage.
- **Round-trips.** `get(put(content)) == content`, for any bytes (including
  empty).
- **Range reads.** `get_range(offset, len)` returns
  `content[offset .. offset+len]`, **clamped** to the end of the content; an
  `offset` at or past the end yields an empty `Vec`.
- **`size` / `has`.** `size` is the content length; `has` is true only for
  stored content; `get`/`size` on an unstored id return
  [`StoreError::NotFound`].
- **Distinctness.** Different content yields different ids (BLAKE3).

What the suite does **not** cover is backend-specific behaviour —
deduplication on disk, the storage layout, concurrency under your I/O model.
Write your own tests for those (see `FilesystemStore`'s `#[cfg(test)] mod
tests` for examples: identical-content-deduplicates, prefix-sharing).

## Implementing

1. Add `fq-store` as a dependency; define your store type.
2. Implement `#[async_trait] impl ContentStore for YourStore`.
3. Compute ids with `Cid::of(content)`; map your "absent" condition to
   `StoreError::NotFound(cid)` and any corruption to `StoreError::Corrupt`.
4. Deduplication is yours to design — the reference splits content into
   content-defined blocks (FastCDC) and stores each block once by its BLAKE3
   hash, with a per-object manifest of `(block, len)`. A different backend
   may dedup differently (or not at all); the trait does not mandate *how*.

## Running the conformance suite

Add `proptest` and `tokio` as **dev-dependencies**, then invoke the macro in
a test file (`tests/your_backend.rs`), passing an expression that constructs a
store:

```rust
use fq_store::content_store_conformance;

content_store_conformance!(YourStore::new(/* fresh, isolated storage */));
```

That generates the full property-test module (`roundtrip`, `idempotent`,
`range`, `size_and_has`, `distinct`, `content_addressed`), each run over
hundreds of randomized inputs. `cargo test` runs them.

A content-addressed store accumulates content without key collisions, so the
macro shares one store instance across all generated cases — make sure your
constructor expression yields storage that outlives the test (e.g. a
persisted temp dir, not one dropped at the end of the expression).

## Why this matters

The CAS is the layer every other layer (names, GC, extraction, the vector
index) trusts absolutely. Writing the correctness contract *once*, as
executable properties against the trait, means that trust is re-verified for
free the day a second backend appears — which is exactly when subtle storage
bugs would otherwise slip in.

[`ContentStore`]: ../../services/fq-store/src/lib.rs
[`StoreError::NotFound`]: ../../services/fq-store/src/error.rs
