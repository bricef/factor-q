//! The CAS network service, proven against the conformance suite **over the
//! wire**: a `RemoteStore` (tarpc client talking to an in-process server)
//! re-runs the same correctness checks as the in-process filesystem backend.
//! This validates ADR-0023's "same contract, in-process and distributed".
#![cfg(feature = "service")]

use std::sync::Arc;

use fq_store::ContentStore;
use fq_store::conformance;
use fq_store::fs::{ChunkParams, FilesystemStore};
use fq_store::service::{self, RemoteStore};

/// Start a CAS server on an ephemeral localhost port; return its address.
async fn start_server() -> String {
    let dir = tempfile::tempdir().unwrap().keep();
    let store: Arc<dyn ContentStore> = Arc::new(FilesystemStore::with_params(
        dir,
        ChunkParams {
            min: 256,
            avg: 1024,
            max: 4096,
        },
    ));
    let (addr, serving) = service::bind("127.0.0.1:0", store).await.unwrap();
    tokio::spawn(serving);
    addr.to_string()
}

#[tokio::test]
async fn remote_store_passes_conformance_over_the_wire() {
    let addr = start_server().await;
    let store = RemoteStore::connect(&addr).await.unwrap();

    // A spread of sizes: empty, tiny, medium, multi-block, multi-frame.
    let inputs: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"x".to_vec(),
        b"hello content-addressed world".to_vec(),
        (0..50_000u32).map(|i| i as u8).collect(),
        vec![7u8; 200_000],
    ];

    for content in &inputs {
        conformance::roundtrip(&store, content).await.unwrap();
        conformance::idempotent(&store, content).await.unwrap();
        conformance::size_and_has(&store, content).await.unwrap();
        conformance::content_addressed(&store, content)
            .await
            .unwrap();
        let len = content.len() as u64;
        conformance::range(&store, content, 0, len).await.unwrap();
        conformance::range(&store, content, len / 3, len / 2)
            .await
            .unwrap();
        conformance::range(&store, content, len.saturating_sub(10), 100)
            .await
            .unwrap();
    }
    conformance::distinct(&store, b"alpha", b"beta")
        .await
        .unwrap();
    // The reusable aggregate invariant, against this (isolated) remote store.
    conformance::stats_consistent(&store, b"gamma")
        .await
        .unwrap();
}

/// `remove` over the wire — an isolated store, since deletion leaves orphan
/// blocks (reclaimed by the collector, slice 5) that would trip the shared
/// store's `stats_consistent` check.
#[tokio::test]
async fn remote_store_supports_deletion_over_the_wire() {
    let addr = start_server().await;
    let store = RemoteStore::connect(&addr).await.unwrap();
    let big = vec![4u8; 40_000];
    for content in [&b""[..], &b"a deletable object"[..], big.as_slice()] {
        conformance::removal(&store, content).await.unwrap();
    }
}
