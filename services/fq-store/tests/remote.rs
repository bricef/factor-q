//! The CAS network service, proven against the conformance suite **over the
//! wire**: a `RemoteStore` (tarpc client talking to an in-process server)
//! re-runs the same correctness checks as the in-process filesystem backend.
//! This validates ADR-0023's "same contract, in-process and distributed".
#![cfg(feature = "service")]

use std::sync::Arc;

use fq_store::conformance;
use fq_store::fs::{ChunkParams, FilesystemStore};
use fq_store::service::{self, CAS_SERVICE_V2_METHODS, RemoteStore};
use fq_store::{Cid, ContentStore, StoreError};

/// Start a CAS server on an ephemeral localhost port; return its address.
async fn start_server() -> String {
    let dir = tempfile::tempdir().unwrap().keep();
    let store: Arc<dyn ContentStore> =
        Arc::new(FilesystemStore::with_params(dir, ChunkParams::small()));
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

#[test]
fn wire_service_exposes_exactly_the_six_client_verbs() {
    assert_eq!(
        CAS_SERVICE_V2_METHODS,
        ["put", "get", "get_range", "has", "size", "stats"]
    );
}

#[tokio::test]
async fn remote_store_rejects_gc_operations() {
    let addr = start_server().await;
    let store = RemoteStore::connect(&addr).await.unwrap();
    let cid = Cid::of(b"not sent over the wire");

    for (operation, result) in [
        ("remove", store.remove(&cid).await),
        ("has_block", store.has_block(&cid, 0).await.map(|_| ())),
        ("remove_block", store.remove_block(&cid, 0).await),
    ] {
        let StoreError::Unsupported(message) = result.unwrap_err() else {
            panic!("{operation} did not return Unsupported");
        };
        assert_eq!(
            message,
            format!(
                "{operation} is not exposed over the wire until M5 authentication; run the collector in-process"
            )
        );
    }
}
