//! Backend-agnostic conformance suite for [`ContentStore`](crate::ContentStore).
//!
//! These are the correctness properties *every* backend must satisfy. They
//! are expressed as backend-agnostic `async` check functions plus the
//! [`content_store_conformance!`] macro, which drives them with `proptest`.
//! A backend proves itself by invoking the macro in its own tests — see
//! `docs/guide/implementing-a-storage-backend.md`.
//!
//! The check functions take `&dyn ContentStore`, so the suite is reused
//! verbatim across every implementation.

use crate::{Cid, ContentStore};

/// Outcome of a single property check: `Ok(())`, or a human-readable reason.
pub type Check = std::result::Result<(), String>;

fn fail(msg: impl Into<String>) -> Check {
    Err(msg.into())
}

/// `get(put(content)) == content`.
pub async fn roundtrip<S: ContentStore + ?Sized>(store: &S, content: &[u8]) -> Check {
    let cid = store.put(content).await.map_err(|e| format!("put: {e}"))?;
    let got = store.get(&cid).await.map_err(|e| format!("get: {e}"))?;
    if got != content {
        return fail(format!(
            "roundtrip mismatch: got {} bytes, expected {}",
            got.len(),
            content.len()
        ));
    }
    Ok(())
}

/// `put` is deterministic and idempotent: the same content yields the same
/// `Cid` every time, and remains readable after a re-`put`.
pub async fn idempotent<S: ContentStore + ?Sized>(store: &S, content: &[u8]) -> Check {
    let first = store
        .put(content)
        .await
        .map_err(|e| format!("put #1: {e}"))?;
    let second = store
        .put(content)
        .await
        .map_err(|e| format!("put #2: {e}"))?;
    if first != second {
        return fail("put is non-deterministic: two cids for identical content");
    }
    let got = store.get(&first).await.map_err(|e| format!("get: {e}"))?;
    if got != content {
        return fail("content changed after a re-put");
    }
    Ok(())
}

/// `get_range(offset, len) == content[offset .. offset+len]`, clamped to the
/// end of the content.
pub async fn range<S: ContentStore + ?Sized>(
    store: &S,
    content: &[u8],
    offset: u64,
    len: u64,
) -> Check {
    let cid = store.put(content).await.map_err(|e| format!("put: {e}"))?;
    let got = store
        .get_range(&cid, offset, len)
        .await
        .map_err(|e| format!("get_range: {e}"))?;
    let start = (offset as usize).min(content.len());
    let end = offset.saturating_add(len).min(content.len() as u64) as usize;
    let want = &content[start..end];
    if got != want {
        return fail(format!(
            "range[{offset}..+{len}] mismatch: got {} bytes, expected {}",
            got.len(),
            want.len()
        ));
    }
    Ok(())
}

/// `size` reports the content length, and `has` is true for stored content
/// and false for an unstored id.
pub async fn size_and_has<S: ContentStore + ?Sized>(store: &S, content: &[u8]) -> Check {
    let cid = store.put(content).await.map_err(|e| format!("put: {e}"))?;
    let size = store.size(&cid).await.map_err(|e| format!("size: {e}"))?;
    if size != content.len() as u64 {
        return fail(format!("size {} != content length {}", size, content.len()));
    }
    if !store.has(&cid).await.map_err(|e| format!("has: {e}"))? {
        return fail("has() is false for stored content");
    }
    const ABSENT: &[u8] = b"fq-store conformance: a deliberately unstored sentinel value";
    if content != ABSENT
        && store
            .has(&Cid::of(ABSENT))
            .await
            .map_err(|e| format!("has(absent): {e}"))?
    {
        return fail("has() is true for unstored content");
    }
    Ok(())
}

/// Distinct content yields distinct ids.
pub async fn distinct<S: ContentStore + ?Sized>(store: &S, a: &[u8], b: &[u8]) -> Check {
    if a == b {
        return Ok(());
    }
    let ca = store.put(a).await.map_err(|e| format!("put a: {e}"))?;
    let cb = store.put(b).await.map_err(|e| format!("put b: {e}"))?;
    if ca == cb {
        return fail("distinct content collided onto a single cid");
    }
    Ok(())
}

/// `put` returns the content's address: `put(content) == Cid::of(content)`,
/// so any party can derive the id from the bytes alone — the defining
/// property of content addressing.
pub async fn content_addressed<S: ContentStore + ?Sized>(store: &S, content: &[u8]) -> Check {
    let cid = store.put(content).await.map_err(|e| format!("put: {e}"))?;
    if cid != Cid::of(content) {
        return fail("put did not return Cid::of(content) — not content-addressed");
    }
    Ok(())
}

/// Store statistics are internally consistent: physical bytes never exceed
/// logical (dedup cannot grow content), every block is referenced, and the
/// dedup ratio is at least 1 once content exists.
///
/// Run this against an **isolated** store: it scans the whole store, so a
/// concurrent writer would race it — which is why it is not wired into the
/// shared (parallel) `content_store_conformance!` suite. A backend exercises
/// it in a dedicated test (see the filesystem backend).
pub async fn stats_consistent<S: ContentStore + ?Sized>(store: &S, content: &[u8]) -> Check {
    store.put(content).await.map_err(|e| format!("put: {e}"))?;
    let stats = store.stats().await.map_err(|e| format!("stats: {e}"))?;
    if stats.physical_bytes > stats.logical_bytes {
        return fail(format!(
            "physical {} exceeds logical {}",
            stats.physical_bytes, stats.logical_bytes
        ));
    }
    if stats.blocks > stats.block_refs {
        return fail(format!(
            "blocks {} exceeds block refs {}",
            stats.blocks, stats.block_refs
        ));
    }
    if stats.dedup_ratio() < 1.0 {
        return fail(format!("dedup ratio {} < 1.0", stats.dedup_ratio()));
    }
    if stats.objects == 0 {
        return fail("objects is 0 after a put");
    }
    Ok(())
}

/// `blocks` enumerates an object's dedup units: the object must exist, the
/// result is non-empty for non-empty content, and it is stable across calls.
pub async fn blocks_enumerated<S: ContentStore + ?Sized>(store: &S, content: &[u8]) -> Check {
    let cid = store.put(content).await.map_err(|e| format!("put: {e}"))?;
    let blocks = store
        .blocks(&cid)
        .await
        .map_err(|e| format!("blocks: {e}"))?;
    if !content.is_empty() && blocks.is_empty() {
        return fail("blocks() returned no units for non-empty content");
    }
    let again = store
        .blocks(&cid)
        .await
        .map_err(|e| format!("blocks (again): {e}"))?;
    if blocks != again {
        return fail("blocks() is not stable across calls");
    }
    Ok(())
}

/// `remove` deletes an object: afterwards `has` is false and `get` is
/// `NotFound`; the object's blocks are **not** removed (they are
/// reference-counted and reclaimed separately); and removing an absent object
/// is a no-op. Destructive — run against an **isolated** store.
pub async fn removal<S: ContentStore + ?Sized>(store: &S, content: &[u8]) -> Check {
    let cid = store.put(content).await.map_err(|e| format!("put: {e}"))?;
    if !store.has(&cid).await.map_err(|e| format!("has: {e}"))? {
        return fail("absent right after put");
    }
    store
        .remove(&cid)
        .await
        .map_err(|e| format!("remove: {e}"))?;
    if store
        .has(&cid)
        .await
        .map_err(|e| format!("has after remove: {e}"))?
    {
        return fail("still present after remove");
    }
    store
        .remove(&cid)
        .await
        .map_err(|e| format!("remove (idempotent): {e}"))?;
    Ok(())
}

/// `has_block` reflects block presence and `remove_block` deletes it
/// idempotently: after `put` every enumerated block is present; after
/// `remove_block` the target is absent; a second `remove_block` is a no-op.
/// Destructive (and uses sub-block addressing) — run against an **isolated**
/// store whose `blocks` enumerates real blocks.
pub async fn block_removal<S: ContentStore + ?Sized>(store: &S, content: &[u8]) -> Check {
    if content.is_empty() {
        return Ok(()); // empty content has no blocks
    }
    let cid = store.put(content).await.map_err(|e| format!("put: {e}"))?;
    let blocks = store
        .blocks(&cid)
        .await
        .map_err(|e| format!("blocks: {e}"))?;
    for b in &blocks {
        if !store
            .has_block(b, 0)
            .await
            .map_err(|e| format!("has_block: {e}"))?
        {
            return fail(format!("block {b} absent after put"));
        }
    }
    let target = blocks[0];
    store
        .remove_block(&target, 0)
        .await
        .map_err(|e| format!("remove_block: {e}"))?;
    if store
        .has_block(&target, 0)
        .await
        .map_err(|e| format!("has_block after remove: {e}"))?
    {
        return fail("block present after remove_block");
    }
    store
        .remove_block(&target, 0)
        .await
        .map_err(|e| format!("remove_block (idempotent): {e}"))?;
    Ok(())
}

/// Generate the full `ContentStore` conformance test suite for a backend.
///
/// `$make` is an expression that constructs a store (a content-addressed
/// store accumulates content without key collisions, so one instance is
/// shared across all generated cases). The invoking crate must have
/// `proptest` and `tokio` as dev-dependencies.
///
/// ```ignore
/// content_store_conformance!(
///     fq_store::fs::FilesystemStore::new(tempfile::tempdir().unwrap().keep())
/// );
/// ```
#[macro_export]
macro_rules! content_store_conformance {
    ($make:expr) => {
        mod content_store_conformance {
            use super::*;
            use ::proptest::prelude::*;

            fn store() -> &'static dyn $crate::ContentStore {
                static STORE: std::sync::OnceLock<Box<dyn $crate::ContentStore>> =
                    std::sync::OnceLock::new();
                STORE.get_or_init(|| Box::new($make)).as_ref()
            }

            fn rt() -> &'static ::tokio::runtime::Runtime {
                static RT: std::sync::OnceLock<::tokio::runtime::Runtime> =
                    std::sync::OnceLock::new();
                RT.get_or_init(|| ::tokio::runtime::Runtime::new().unwrap())
            }

            proptest! {
                #[test]
                fn roundtrip(content in prop::collection::vec(any::<u8>(), 0..32768usize)) {
                    if let Err(e) = rt().block_on($crate::conformance::roundtrip(store(), &content)) {
                        prop_assert!(false, "{}", e);
                    }
                }

                #[test]
                fn idempotent(content in prop::collection::vec(any::<u8>(), 0..32768usize)) {
                    if let Err(e) = rt().block_on($crate::conformance::idempotent(store(), &content)) {
                        prop_assert!(false, "{}", e);
                    }
                }

                #[test]
                fn range(
                    content in prop::collection::vec(any::<u8>(), 0..32768usize),
                    offset in 0u64..40000,
                    len in 0u64..40000,
                ) {
                    if let Err(e) = rt().block_on($crate::conformance::range(store(), &content, offset, len)) {
                        prop_assert!(false, "{}", e);
                    }
                }

                #[test]
                fn size_and_has(content in prop::collection::vec(any::<u8>(), 0..32768usize)) {
                    if let Err(e) = rt().block_on($crate::conformance::size_and_has(store(), &content)) {
                        prop_assert!(false, "{}", e);
                    }
                }

                #[test]
                fn distinct(
                    a in prop::collection::vec(any::<u8>(), 0..4096usize),
                    b in prop::collection::vec(any::<u8>(), 0..4096usize),
                ) {
                    if let Err(e) = rt().block_on($crate::conformance::distinct(store(), &a, &b)) {
                        prop_assert!(false, "{}", e);
                    }
                }

                #[test]
                fn content_addressed(content in prop::collection::vec(any::<u8>(), 0..32768usize)) {
                    if let Err(e) = rt().block_on($crate::conformance::content_addressed(store(), &content)) {
                        prop_assert!(false, "{}", e);
                    }
                }

                #[test]
                fn blocks_enumerated(content in prop::collection::vec(any::<u8>(), 0..32768usize)) {
                    if let Err(e) = rt().block_on($crate::conformance::blocks_enumerated(store(), &content)) {
                        prop_assert!(false, "{}", e);
                    }
                }
            }
        }
    };
}
