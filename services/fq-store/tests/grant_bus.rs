//! The NATS-backed grant bus, proven against a real broker (feature `bus`):
//! append to the local log, drain to JetStream, read back, and compare with
//! the log's own replay. Requires NATS at `FQ_NATS_URL` (default
//! `nats://127.0.0.1:4222`) — `just infra-up` provides it, as in CI.
#![cfg(feature = "bus")]

use std::collections::BTreeSet;

use fq_store::grant_log::nats::NatsGrantBus;
use fq_store::{Grantor, Principal, Scope, SqliteGrantLog, Verb, WireGrantEvent, drain};
use futures::StreamExt as _;

fn nats_url() -> String {
    std::env::var("FQ_NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into())
}

/// A per-run unique suffix so parallel/repeated runs never share a stream.
fn unique() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{}_{nanos}", std::process::id())
}

#[tokio::test]
async fn drain_publishes_to_jetstream_and_round_trips() {
    let uniq = unique();
    let stream_name = format!("FQ_GRANTS_TEST_{uniq}");
    let prefix = format!("fq.test.grant{uniq}");
    let url = nats_url();

    // Local log with three events: grant, delegation, revocation.
    let dir = tempfile::tempdir().unwrap();
    let log = SqliteGrantLog::open(dir.path().join("grants.db"))
        .await
        .unwrap();
    let id = log
        .append_granted(
            &Grantor::Operator,
            &Principal::Agent("alice".into()),
            &BTreeSet::from([Verb::Read, Verb::Grant]),
            &Scope::Namespace("research".into()),
        )
        .await
        .unwrap();
    log.append_granted(
        &Grantor::Agent("alice".into()),
        &Principal::Agent("bob".into()),
        &BTreeSet::from([Verb::Read]),
        &Scope::Namespace("research.papers".into()),
    )
    .await
    .unwrap();
    log.append_revoked(id).await.unwrap();

    // Drain to a real broker.
    let bus = NatsGrantBus::connect(&url, &stream_name, &prefix)
        .await
        .expect("NATS reachable (just infra-up)");
    assert_eq!(drain(&log, &bus).await.unwrap(), 3);
    assert!(log.pending().await.unwrap().is_empty());

    // Read back from JetStream and compare with the log's own replay.
    let client = async_nats::connect(&url).await.unwrap();
    let js = async_nats::jetstream::new(client);
    let stream = js.get_stream(&stream_name).await.unwrap();
    let consumer = stream
        .create_consumer(async_nats::jetstream::consumer::pull::Config {
            durable_name: None,
            ..Default::default()
        })
        .await
        .unwrap();
    let mut fetched = Vec::new();
    let mut messages = consumer.fetch().max_messages(3).messages().await.unwrap();
    while let Some(message) = messages.next().await {
        let message = message.unwrap();
        let envelope: WireGrantEvent = serde_json::from_slice(&message.payload).unwrap();
        fetched.push(envelope);
    }
    assert_eq!(fetched.len(), 3);
    let replayed = log.replay().await.unwrap();
    for (wire, local) in fetched.iter().zip(&replayed) {
        assert_eq!(&wire.event, local, "the bus feed mirrors the local log");
    }
    assert_eq!(fetched[0].schema_id, "factor-q/granted@1");
    assert_eq!(fetched[1].schema_id, "factor-q/delegated@1");
    assert_eq!(fetched[2].schema_id, "factor-q/revoked@1");

    // Tidy up the test stream.
    js.delete_stream(&stream_name).await.unwrap();
}
